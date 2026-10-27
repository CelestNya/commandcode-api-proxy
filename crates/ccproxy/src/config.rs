//! Config loading: CLI flags > env vars > defaults, with the same
//! per-field fallbacks as the Node `loadConfig()` (src/config.ts).

use std::path::Path;

/// Hardcoded CLI version fallback. The real CLI ships frequent releases;
/// CC's server actively blocks requests whose version looks stale or absent,
/// so this must stay at or above the server's `minVersion`. It is only used
/// when the startup npm lookup fails — keep it current when the CLI bumps.
pub const DEFAULT_CC_VERSION: &str = "1.54.1";
pub const DEFAULT_CC_API_BASE: &str = "https://api.commandcode.ai";

/// Env overrides for packaged resource directories (D7). The packager injects
/// the OS-appropriate locations into these variables; the code reads env first
/// and falls back to the portable exe-adjacent layout. There is deliberately
/// *no* platform detection here — the same variables work on Windows and Linux.
pub const WEBUI_DIR_ENV: &str = "CC_WEBUI_DIR";
pub const DATA_DIR_ENV: &str = "CC_DATA_DIR";
pub const LOG_DIR_ENV: &str = "CC_LOG_DIR";
pub const TRAY_LOG_DIR_ENV: &str = "CC_TRAY_LOG_DIR";

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u64 = 8787;
const MAX_BODY_BYTES_DEFAULT: u64 = 10 * 1024 * 1024;
/// Upstream (CC API) hard limit measured in production: bodies of 30 MB are
/// accepted, 32 MB is rejected with `413 Payload Too Large` ("Request body is
/// 32.0 MB, over the 32 MB limit for one request (33554432 bytes)"). The
/// ceiling must not exceed it, otherwise an allowed body would be rejected
/// upstream with a confusing 413 after passing the proxy's own guard.
const MAX_BODY_BYTES_MAX: u64 = 32 * 1024 * 1024;
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
    /// Inline-screenshot quality compression. Deterministic and cache-safe by
    /// contract (`image_compress` module docs); off by default.
    pub image_compress: crate::image_compress::ImageCompressConfig,
    /// Outbound proxy policy. `default` probes the system proxy and falls back
    /// to direct; `direct` never uses one; an explicit URL always does, with no
    /// fallback. See [`ProxySetting`].
    pub proxy: ProxySetting,
    /// Hosts that bypass the proxy even when one is set (`NO_PROXY` in the
    /// config file, comma-separated). Loopback is always bypassed regardless.
    pub no_proxy: Vec<String>,
}

/// The defaults for the proxy policy, so a struct literal (mostly test
/// fixtures) can spread them instead of repeating the pair.
impl Default for ProxySetting {
    fn default() -> Self {
        Self::Default
    }
}

/// A fully-defaulted config, for fixtures and embedders. The values mirror
/// `load`'s own defaults so a spread-in literal behaves like an unconfigured
/// process.
impl Default for Config {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.into(),
            port: DEFAULT_PORT as u16,
            cc_api_base: DEFAULT_CC_API_BASE.into(),
            cc_version: DEFAULT_CC_VERSION.into(),
            log_level: "info".into(),
            cors_origin: "*".into(),
            upstream_timeout_ms: 600_000,
            idle_timeout_ms: 120_000,
            no_output_timeout_ms: NO_OUTPUT_TIMEOUT_DEFAULT_MS,
            no_output_retries: NO_OUTPUT_RETRIES_DEFAULT,
            max_body_bytes: MAX_BODY_BYTES_DEFAULT,
            image_compress: crate::image_compress::ImageCompressConfig::default(),
            proxy: ProxySetting::Default,
            no_proxy: Vec::new(),
        }
    }
}

/// The three states of the outbound-proxy policy.
///
/// The distinction that matters is fallback: `Default` is a *preference* (probe
/// the system setting, and if it does not work go direct), while `Explicit` is
/// an *instruction* (use this proxy or fail — a misconfigured proxy that
/// silently went direct would hide the mistake and look like a flaky upstream).
#[derive(Debug, Clone, PartialEq)]
pub enum ProxySetting {
    /// Probe the system proxy at startup; use it only if a probe succeeds.
    Default,
    /// Never use a proxy.
    Direct,
    /// Always use this proxy.
    Explicit(String),
}

impl ProxySetting {
    /// Parse the config-file / env spelling. `default`/`auto`/empty → Default,
    /// `direct`/`off`/`none` → Direct, anything else is read as a proxy URL.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let t = raw.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("default") || t.eq_ignore_ascii_case("auto") {
            Self::Default
        } else if t.eq_ignore_ascii_case("direct")
            || t.eq_ignore_ascii_case("off")
            || t.eq_ignore_ascii_case("none")
        {
            Self::Direct
        } else {
            Self::Explicit(normalize_proxy_url(t))
        }
    }
}

/// A bare `host:port` is an HTTP proxy; anything with a scheme is kept as-is.
fn normalize_proxy_url(url: &str) -> String {
    if url.contains("://") {
        url.to_string()
    } else {
        format!("http://{url}")
    }
}

/// Environment lookup, abstracted for tests.
pub type EnvLookup<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// The config file, read from `ccproxy.json` beside the executable.
///
/// Explicit file-based configuration rather than more environment variables: a
/// proxy setting is something a person edits once and must be able to read back,
/// and the tray's environment is not visible to whoever debugs a misbehaving
/// install. Precedence is CLI > env > file > default, so an operator can still
/// override a bad file without editing it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Raw proxy string: `default` / `direct` / a URL.
    pub proxy: Option<String>,
    pub no_proxy: Option<String>,
    pub log_level: Option<String>,
    pub cc_api_base: Option<String>,
    pub cc_version: Option<String>,
    pub upstream_timeout_ms: Option<u64>,
    pub idle_timeout_ms: Option<u64>,
    pub no_output_timeout_ms: Option<u64>,
    pub no_output_retries: Option<u64>,
    pub max_body_bytes: Option<u64>,
    pub image_compress: Option<bool>,
    pub image_compress_quality: Option<u64>,
    pub image_compress_min_bytes: Option<u64>,
}

impl FileConfig {
    /// Parse the config file's JSON. Unknown keys are ignored so a newer file
    /// read by an older binary degrades instead of failing to start; a value of
    /// the wrong type is likewise skipped rather than fatal.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
            return Self::default();
        };
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned);
        let n = |k: &str| v.get(k).and_then(serde_json::Value::as_u64);
        Self {
            host: s("host"),
            port: n("port").and_then(|p| u16::try_from(p).ok()),
            proxy: s("proxy"),
            no_proxy: s("noProxy").or_else(|| s("no_proxy")),
            log_level: s("logLevel").or_else(|| s("log_level")),
            cc_api_base: s("ccApiBase"),
            cc_version: s("ccVersion"),
            upstream_timeout_ms: n("upstreamTimeoutMs"),
            idle_timeout_ms: n("idleTimeoutMs"),
            no_output_timeout_ms: n("noOutputTimeoutMs"),
            no_output_retries: n("noOutputRetries"),
            max_body_bytes: n("maxBodyBytes"),
            image_compress: v.get("imageCompress").and_then(serde_json::Value::as_bool),
            image_compress_quality: n("imageCompressQuality"),
            image_compress_min_bytes: n("imageCompressMinBytes"),
        }
    }
}

/// The config file for this process: `ccproxy.json` beside the executable.
///
/// Returns an empty config when the file is absent or unreadable — a missing
/// file is the normal case, not an error, and a broken one must not stop the
/// proxy from serving (the environment still applies).
#[must_use]
pub fn load_file_config() -> FileConfig {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("ccproxy.json")));
    let Some(path) = path else {
        return FileConfig::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => FileConfig::parse(&text),
        Err(_) => FileConfig::default(),
    }
}

pub fn load(argv: &[String], env: &EnvLookup) -> Config {
    load_with_file(argv, env, &load_file_config())
}

/// Load with an explicit file config, so precedence is testable without
/// touching the filesystem or the process environment.
pub fn load_with_file(argv: &[String], env: &EnvLookup, file: &FileConfig) -> Config {
    let cli = parse_cli_args(argv);
    // A flag without a value parses as "true"; host/port treat that as missing.
    let cli_host = cli.host.as_deref().filter(|v| *v != "true");
    let cli_port = cli.port.as_deref().filter(|v| *v != "true");

    let env_host = env("HOST");
    let env_port = env("PORT");
    let host = parse_host(cli_host.or(env_host.as_deref()).or(file.host.as_deref()));
    let port = parse_port(
        cli_port.or(env_port.as_deref()),
        file.port.map_or(DEFAULT_PORT, u64::from),
    );
    let cc_api_base = non_empty(env("CC_API_BASE").as_deref())
        .or_else(|| non_empty(file.cc_api_base.as_deref()))
        .unwrap_or_else(|| DEFAULT_CC_API_BASE.into());
    let cc_version = non_empty(env("CC_CLI_VERSION").as_deref())
        .or_else(|| non_empty(file.cc_version.as_deref()))
        .unwrap_or_else(|| DEFAULT_CC_VERSION.into());
    let log_level = non_empty(env("LOG_LEVEL").as_deref())
        .or_else(|| non_empty(file.log_level.as_deref()))
        .unwrap_or_else(|| "info".into());
    // Empty string is meaningful: it disables CORS entirely.
    let cors_origin = env("CORS_ORIGIN").unwrap_or_else(|| "*".into());
    // Each numeric setting resolves env-first, then the file, then the default;
    // the clamp is applied whichever source won.
    let max_body_bytes = resolve_u64(
        env("CC_MAX_BODY_BYTES").as_deref(),
        file.max_body_bytes,
        MAX_BODY_BYTES_DEFAULT,
        |v| v.floor() as u64,
        |v| v.min(MAX_BODY_BYTES_MAX),
    );
    let upstream_timeout_ms = resolve_u64(
        env("CC_UPSTREAM_TIMEOUT_MS").as_deref(),
        file.upstream_timeout_ms,
        600_000,
        |v| v.floor() as u64,
        |v| v.min(TIMEOUT_MAX_MS),
    );
    let idle_timeout_ms = resolve_u64(
        env("CC_IDLE_TIMEOUT_MS").as_deref(),
        file.idle_timeout_ms,
        120_000,
        |v| v.floor() as u64,
        |v| v.min(TIMEOUT_MAX_MS),
    );
    let no_output_timeout_ms = resolve_u64(
        env("CC_NO_OUTPUT_TIMEOUT_MS").as_deref(),
        file.no_output_timeout_ms,
        NO_OUTPUT_TIMEOUT_DEFAULT_MS,
        |v| v.floor() as u64,
        |v| v.min(TIMEOUT_MAX_MS),
    );
    let no_output_retries = resolve_u64(
        env("CC_NO_OUTPUT_RETRIES").as_deref(),
        file.no_output_retries,
        NO_OUTPUT_RETRIES_DEFAULT,
        |v| v.floor() as u64,
        |v| v.min(NO_OUTPUT_RETRIES_MAX),
    );

    // Image compression: env > file > default (off). The bool accepts the
    // usual words in the env ("true"/"1"/"on"...); the file uses JSON booleans.
    let image_compress = crate::image_compress::ImageCompressConfig {
        enabled: env("CC_IMAGE_COMPRESS")
            .as_deref()
            .and_then(parse_bool_word)
            .or(file.image_compress)
            .unwrap_or(false),
        quality: resolve_u64(
            env("CC_IMAGE_COMPRESS_QUALITY").as_deref(),
            file.image_compress_quality,
            u64::from(crate::image_compress::DEFAULT_QUALITY),
            |v| v.floor() as u64,
            |v| v.clamp(1, 100),
        ) as u8,
        min_input_bytes: file.image_compress_min_bytes.unwrap_or(
            crate::image_compress::DEFAULT_MIN_BYTES,
        ),
    };

    // Proxy: env (CC_PROXY) > file (`proxy`) > default. The env override stays
    // honoured because it is what operators already use to force direct.
    let proxy_raw = non_empty(env("CC_PROXY").as_deref()).or_else(|| file.proxy.clone());
    let proxy = proxy_raw.map_or(ProxySetting::Default, |raw| ProxySetting::parse(&raw));
    let no_proxy =
        parse_no_proxy(non_empty(env("NO_PROXY").as_deref()).or_else(|| file.no_proxy.clone()));

    // With compression enabled the translated body can only shrink, so the
    // incoming guard must not veto screenshot-heavy requests before the
    // translator ever sees them: raise the guard to the hard ceiling. The
    // upstream 32 MB limit still bounds what actually goes out.
    let max_body_bytes = if image_compress.enabled {
        max_body_bytes.max(MAX_BODY_BYTES_MAX)
    } else {
        max_body_bytes
    };

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
        image_compress,
        proxy,
        no_proxy,
    }
}

/// `1`/`true`/`yes`/`on` -> `Some(true)`; `0`/`false`/`no`/`off` ->
/// `Some(false)`; anything else (including empty) -> `None`, so a typoed env
/// value falls through to the file rather than silently disabling a feature.
fn parse_bool_word(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Split a `NO_PROXY` list into trimmed, non-empty entries.
fn parse_no_proxy(raw: Option<String>) -> Vec<String> {
    raw.map(|text| {
        text.split(',')
            .flat_map(|p| p.split(';'))
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

/// Resolve one numeric setting from env-first, then the file value, then the
/// default, applying `clamp` to whichever source won.
///
/// An env value that does not parse falls through to the file rather than
/// jumping to the default: a typo in an override should not silently discard a
/// deliberate file setting.
fn resolve_u64(
    env_raw: Option<&str>,
    file_value: Option<u64>,
    fallback: u64,
    normalize: impl Fn(f64) -> u64,
    clamp: impl Fn(u64) -> u64,
) -> u64 {
    if let Some(raw) = env_raw {
        if !raw.trim().is_empty() {
            if let Some(n) = js_number(raw).filter(|n| n.is_finite() && *n >= 0.0) {
                return clamp(normalize(n));
            }
        }
    }
    file_value.map_or(fallback, clamp)
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

/// The proxy log path for this process. `CC_LOG_DIR` (the packaged layout)
/// names the log directory directly; otherwise the path is next to
/// `current_exe`, namespaced when `CC_TRAY_NS` is set (same variable the
/// ledger uses).
#[must_use]
pub fn log_path() -> std::path::PathBuf {
    let env_dir = env_dir(LOG_DIR_ENV);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let ns = std::env::var("CC_TRAY_NS").ok();
    log_path_with(exe_dir.as_deref(), ns.as_deref(), env_dir.as_deref())
}

/// The proxy log path with no env override — the legacy decision, kept for
/// existing callers and tests.
#[must_use]
pub fn log_path_from(exe_dir: Option<&Path>, ns: Option<&str>) -> std::path::PathBuf {
    log_path_with(exe_dir, ns, None)
}

/// The decision, with `env_dir` as the optional override. When set, `env_dir`
/// *is* the log directory (no `logs/` suffix — the packager named it already);
/// the namespace still nests under it so an isolated instance cannot read
/// production's file or vice versa.
fn log_path_with(
    exe_dir: Option<&Path>,
    ns: Option<&str>,
    env_dir: Option<&Path>,
) -> std::path::PathBuf {
    match env_dir {
        Some(dir) => match ns.filter(|v| !v.trim().is_empty()) {
            Some(ns) => dir.join(format!("test-{ns}")).join("proxy.log"),
            None => dir.join("proxy.log"),
        },
        None => {
            let base = exe_dir.map_or_else(|| std::path::PathBuf::from("."), Path::to_path_buf);
            let logs = base.join("logs");
            let dir = match ns.filter(|v| !v.trim().is_empty()) {
                Some(ns) => logs.join(format!("test-{ns}")),
                None => logs,
            };
            dir.join("proxy.log")
        }
    }
}

/// The tray log path for this process. `CC_TRAY_LOG_DIR` (the packaged layout)
/// names the tray log directory directly; otherwise the proxy runs from
/// `<root>/service`, so the package root is its exe directory's parent; a proxy
/// launched straight from the root has no separate tray log, and the path
/// simply will not exist.
#[must_use]
pub fn tray_log_path() -> std::path::PathBuf {
    let env_dir = env_dir(TRAY_LOG_DIR_ENV);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let root = exe_dir.as_deref().and_then(Path::parent);
    let ns = std::env::var("CC_TRAY_NS").ok();
    tray_log_path_with(root, ns.as_deref(), env_dir.as_deref())
}

/// The tray log path with no env override — the legacy decision, kept for
/// existing callers and tests.
#[must_use]
pub fn tray_log_path_from(root: Option<&Path>, ns: Option<&str>) -> std::path::PathBuf {
    tray_log_path_with(root, ns, None)
}

/// The decision, with `env_dir` as the optional override; when set it *is* the
/// log directory (no `logs/` suffix).
fn tray_log_path_with(
    root: Option<&Path>,
    ns: Option<&str>,
    env_dir: Option<&Path>,
) -> std::path::PathBuf {
    match env_dir {
        Some(dir) => match ns.filter(|v| !v.trim().is_empty()) {
            Some(ns) => dir.join(format!("test-{ns}")).join("tray.log"),
            None => dir.join("tray.log"),
        },
        None => {
            let base = root.map_or_else(|| std::path::PathBuf::from("."), Path::to_path_buf);
            let logs = base.join("logs");
            let dir = match ns.filter(|v| !v.trim().is_empty()) {
                Some(ns) => logs.join(format!("test-{ns}")),
                None => logs,
            };
            dir.join("tray.log")
        }
    }
}

/// The loose WebUI sources: `CC_WEBUI_DIR` when set (the packaged layout
/// injects it), else `webui/` beside the executable when that directory exists.
///
/// The proxy serves the panel from disk (front-end split: nothing is compiled
/// into the binary), so a stylesheet can be edited and just reloaded. `None`
/// means "no webui available" — the panel routes 404, not an error.
#[must_use]
pub fn webui_site_dir() -> Option<std::path::PathBuf> {
    let env_dir = env_dir(WEBUI_DIR_ENV);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    webui_site_dir_from(exe_dir.as_deref(), env_dir.as_deref())
}

/// The decision with inputs passed in, so it is testable without the process
/// environment or the filesystem. The env override wins over the exe-adjacent
/// fallback; either way the directory must actually contain `index.html`.
#[must_use]
pub fn webui_site_dir_from(
    exe_dir: Option<&Path>,
    env_dir: Option<&Path>,
) -> Option<std::path::PathBuf> {
    if let Some(dir) = env_dir {
        if dir.join("index.html").is_file() {
            return Some(dir.to_path_buf());
        }
    }
    let dir = exe_dir?.join("webui");
    dir.join("index.html").is_file().then_some(dir)
}

/// Read a directory env var, treating empty/whitespace as absent.
fn env_dir(key: &str) -> Option<std::path::PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(std::path::PathBuf::from)
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
    fn image_compress_resolves_env_then_file_then_default() {
        use crate::image_compress::{DEFAULT_MIN_BYTES, DEFAULT_QUALITY};

        // Default: off, stock quality.
        let c = load(&[], &no_env);
        assert!(!c.image_compress.enabled);
        assert_eq!(c.image_compress.quality, DEFAULT_QUALITY);
        assert_eq!(c.image_compress.min_input_bytes, DEFAULT_MIN_BYTES);

        // File on, with a quality.
        let file = FileConfig::parse(r#"{"imageCompress": true, "imageCompressQuality": 60}"#);
        let c = load_with_file(&[], &no_env, &file);
        assert!(c.image_compress.enabled);
        assert_eq!(c.image_compress.quality, 60);

        // Env wins over the file in either direction; a switch override leaves
        // the file's quality alone.
        let c = load_with_file(&[], &env_of(&[("CC_IMAGE_COMPRESS", "off")]), &file);
        assert!(!c.image_compress.enabled);
        assert_eq!(c.image_compress.quality, 60);

        // The env bool accepts words, and a typo falls through to the file
        // rather than silently flipping to the default.
        let on_file = FileConfig::parse(r#"{"imageCompress": true}"#);
        let c = load_with_file(
            &[],
            &env_of(&[("CC_IMAGE_COMPRESS", "ture")]),
            &on_file,
        );
        assert!(c.image_compress.enabled);

        // Quality clamps into 1..=100 whichever source won.
        let c = load_with_file(
            &[],
            &env_of(&[("CC_IMAGE_COMPRESS_QUALITY", "0")]),
            &on_file,
        );
        assert_eq!(c.image_compress.quality, 1);

        // Enabling the compressor raises the incoming body guard to the hard
        // ceiling (the translated body only shrinks); disabling leaves the
        // configured limit alone.
        let c = load_with_file(&[], &env_of(&[("CC_IMAGE_COMPRESS", "on")]), &FileConfig::default());
        assert_eq!(c.max_body_bytes, MAX_BODY_BYTES_MAX);
        let c = load_with_file(&[], &no_env, &FileConfig::default());
        assert_eq!(c.max_body_bytes, MAX_BODY_BYTES_DEFAULT);
    }

    #[test]
    fn cli_overrides_env() {
        let argv = svec(["--port", "9999", "--host", "0.0.0.0"]);
        let c = load(&argv, &env_of(&[("PORT", "7777"), ("HOST", "10.0.0.1")]));
        assert_eq!(c.port, 9999);
        assert_eq!(c.host, "0.0.0.0");
    }

    #[test]
    fn the_proxy_policy_parses_its_three_states() {
        assert_eq!(ProxySetting::parse("default"), ProxySetting::Default);
        assert_eq!(ProxySetting::parse(""), ProxySetting::Default);
        assert_eq!(ProxySetting::parse("auto"), ProxySetting::Default);
        assert_eq!(ProxySetting::parse("direct"), ProxySetting::Direct);
        assert_eq!(ProxySetting::parse("off"), ProxySetting::Direct);
        assert_eq!(ProxySetting::parse("none"), ProxySetting::Direct);
        // A URL (with or without a scheme) is explicit, and always proxied.
        assert_eq!(
            ProxySetting::parse("http://127.0.0.1:7897"),
            ProxySetting::Explicit("http://127.0.0.1:7897".into())
        );
        assert_eq!(
            ProxySetting::parse("127.0.0.1:7897"),
            ProxySetting::Explicit("http://127.0.0.1:7897".into())
        );
    }

    #[test]
    fn the_config_file_is_read_and_gives_way_to_cli_and_env() {
        let file = FileConfig::parse(
            r#"{
                "host": "0.0.0.0",
                "port": 9000,
                "proxy": "http://127.0.0.1:1080",
                "noProxy": "corp.example; .internal,localhost",
                "logLevel": "debug",
                "idleTimeoutMs": 30000
            }"#,
        );
        assert_eq!(file.host.as_deref(), Some("0.0.0.0"));
        assert_eq!(file.port, Some(9000));
        assert_eq!(file.log_level.as_deref(), Some("debug"));
        assert_eq!(file.idle_timeout_ms, Some(30000));

        // File alone supplies every value.
        let c = load_with_file(&[], &no_env, &file);
        assert_eq!(c.host, "0.0.0.0");
        assert_eq!(c.port, 9000);
        assert_eq!(c.log_level, "debug");
        assert_eq!(c.idle_timeout_ms, 30000);
        assert_eq!(
            c.proxy,
            ProxySetting::Explicit("http://127.0.0.1:1080".into())
        );
        // The list splits on both separators and drops empty pieces.
        assert_eq!(c.no_proxy, vec!["corp.example", ".internal", "localhost"]);

        // CLI beats the file for host/port...
        let c = load_with_file(
            &svec(["--port", "1234"]),
            &env_of(&[("HOST", "10.0.0.1")]),
            &file,
        );
        assert_eq!(c.port, 1234); // CLI wins
        assert_eq!(c.host, "10.0.0.1"); // env wins over file
                                        // ...and an env override beats a file setting for the proxy too.
        let c = load_with_file(&[], &env_of(&[("CC_PROXY", "off")]), &file);
        assert_eq!(c.proxy, ProxySetting::Direct);
    }

    #[test]
    fn a_broken_config_file_does_not_stop_the_proxy() {
        // A missing or malformed file must degrade to defaults, not fail:
        // the environment still applies and the proxy must still serve.
        let file = FileConfig::parse("{ this is not json");
        assert_eq!(file, FileConfig::default());
        let c = load_with_file(&[], &no_env, &file);
        assert_eq!(c.port, 8787);
        assert_eq!(c.proxy, ProxySetting::Default);

        // A value of the wrong type is skipped, not fatal.
        let file = FileConfig::parse(r#"{"port": "not-a-number", "proxy": 42}"#);
        assert_eq!(file.port, None);
        assert_eq!(file.proxy, None);
    }

    #[test]
    fn an_env_typo_falls_through_to_the_file_not_the_default() {
        // A deliberate file value must survive an unparseable env override,
        // otherwise a stray export silently discards it.
        let file = FileConfig::parse(r#"{"idleTimeoutMs": 45000}"#);
        let c = load_with_file(&[], &env_of(&[("CC_IDLE_TIMEOUT_MS", "abc")]), &file);
        assert_eq!(c.idle_timeout_ms, 45000);
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

    #[test]
    fn the_two_logs_land_in_their_own_places() {
        // The proxy writes `logs/proxy.log` beside its exe; the tray writes
        // `logs/tray.log` at the package root. Both split by namespace, so a
        // test instance cannot read production's files — or vice versa.
        let service = Path::new("C:\\pkg\\service");
        assert_eq!(
            log_path_from(Some(service), None),
            service.join("logs").join("proxy.log")
        );
        assert_eq!(
            log_path_from(Some(service), Some("abc")),
            service.join("logs").join("test-abc").join("proxy.log")
        );

        let root = Path::new("C:\\pkg");
        assert_eq!(
            tray_log_path_from(Some(root), None),
            root.join("logs").join("tray.log")
        );
        assert_eq!(
            tray_log_path_from(Some(root), Some("abc")),
            root.join("logs").join("test-abc").join("tray.log")
        );
        // No root (proxy launched with no known package root): a relative path
        // rather than a panic.
        assert_eq!(
            tray_log_path_from(None, None),
            Path::new(".").join("logs").join("tray.log")
        );
    }

    fn svec<const N: usize>(items: [&str; N]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
}
