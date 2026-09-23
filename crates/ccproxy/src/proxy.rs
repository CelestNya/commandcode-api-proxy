//! Outbound proxy selection for every upstream connection.
//!
//! `ureq` only reads the proxy environment variables when built with its
//! `proxy-from-env` feature, and even then its resolution order is wrong here:
//! it prefers `ALL_PROXY` over `HTTPS_PROXY` and has no `NO_PROXY` support at
//! all. The Rust build therefore went direct while the Node build tunnelled
//! through the system proxy — the whole reason a working install "broke" on
//! upgrade. Selection happens here explicitly instead, so what the proxy does
//! is readable from the config file and provable from the startup log.
//!
//! Three policies, from [`crate::config::ProxySetting`]:
//!
//! - `default` — probe the system proxy; keep it only if the probe succeeds,
//!   otherwise fall back to direct. A *preference*: a dead system proxy costs
//!   one probe at startup and then gets out of the way.
//! - `direct` — never proxy.
//! - an explicit URL — always proxy. An *instruction*: a wrong proxy must
//!   surface as errors, not be silently bypassed into looking like a flaky
//!   upstream.
//!
//! `NO_PROXY` is served by holding two agents — proxied and direct — and
//! choosing per URL host, which is the only way to honour the list on `ureq`
//! 2.x (it has no per-request bypass hook). Loopback is always exempt.

use crate::config::{Config, ProxySetting};
use std::sync::OnceLock;
use std::time::Duration;

/// How long a proxy probe may take before it is judged unusable.
///
/// Short on purpose: this runs once at startup, and a proxy too slow to
/// complete a small request would make every real request worse than going
/// direct.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// What a probe fetches. A real host on the far side of the proxy that answers
/// cheaply and needs no API key.
const PROBE_URL: &str = "https://api.commandcode.ai/health";

/// The resolved egress plan: how to reach the network, plus the log note.
pub struct Egress {
    /// The proxy to use, if one was selected. `None` means direct for every
    /// host, which is also the fallback the `default` policy takes.
    proxy: Option<ureq::Proxy>,
    using_proxy: bool,
    no_proxy: Vec<String>,
    /// One-line description of the decision, printed at startup.
    pub note: String,
}

impl Egress {
    /// Whether `url`'s host is exempt from the proxy.
    fn bypasses(&self, url: &str) -> bool {
        if !self.using_proxy {
            return true;
        }
        let Some(host) = host_of(url) else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        if host == "localhost" || host == "127.0.0.1" || host == "::1" {
            return true;
        }
        self.no_proxy.iter().any(|e| {
            let e = e.trim_start_matches('.').to_ascii_lowercase();
            host == e || host.ends_with(&format!(".{e}"))
        })
    }

    /// The proxy to apply to `url`: `Some` only when one is configured and the
    /// host is not exempt.
    fn proxy_for(&self, url: &str) -> Option<ureq::Proxy> {
        if self.bypasses(url) {
            None
        } else {
            self.proxy.clone()
        }
    }
}

/// Build an agent for `url` with the egress decision baked in and the given
/// per-request timeouts.
///
/// The timeouts must be fixed at build time: `ureq` 2.x has no per-request
/// connect/read override, and its whole-request `.timeout()` would cap a
/// generation, which is forbidden. So the agent carries them, and callers with
/// a different deadline get their own agent — but always with the *same* proxy
/// decision, which is the invariant that matters.
#[must_use]
pub fn agent_with_timeouts(url: &str, connect_ms: u64, write_ms: u64, read_ms: u64) -> ureq::Agent {
    let proxy = installed().proxy_for(url);
    let builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(connect_ms))
        .timeout_write(Duration::from_millis(write_ms))
        .timeout_read(Duration::from_millis(read_ms));
    match proxy {
        Some(p) => builder.proxy(p).build(),
        None => builder.build(),
    }
}

/// The host out of `https://host:port/path`, lowercased, without the port.
fn host_of(url: &str) -> Option<&str> {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    // A bracketed IPv6 literal keeps its brackets only in the port split.
    if let Some(stripped) = host.strip_prefix('[') {
        return stripped.split(']').next();
    }
    Some(host.split(':').next().unwrap_or(host))
}

static EGRESS: OnceLock<Egress> = OnceLock::new();

/// Resolve and install the egress plan. The first call wins.
pub fn init(config: &Config) -> &'static Egress {
    EGRESS.get_or_init(|| resolve(config))
}

/// The installed plan; falls back to direct when nothing initialised it, so a
/// test or embedder that skipped [`init`] still gets a working agent.
fn installed() -> &'static Egress {
    EGRESS.get_or_init(|| resolve(&fallback_config()))
}

/// The startup description of the chosen egress.
#[must_use]
pub fn note() -> &'static str {
    installed().note.as_str()
}

fn fallback_config() -> Config {
    Config {
        host: "127.0.0.1".into(),
        port: 8787,
        cc_api_base: crate::config::DEFAULT_CC_API_BASE.into(),
        cc_version: crate::config::DEFAULT_CC_VERSION.into(),
        log_level: "info".into(),
        cors_origin: "*".into(),
        upstream_timeout_ms: 600_000,
        idle_timeout_ms: 120_000,
        no_output_timeout_ms: 30_000,
        no_output_retries: 3,
        max_body_bytes: 10 * 1024 * 1024,
        proxy: ProxySetting::Default,
        no_proxy: Vec::new(),
    }
}

fn resolve(config: &Config) -> Egress {
    let no_proxy = config.no_proxy.clone();
    match &config.proxy {
        ProxySetting::Direct => Egress {
            proxy: None,
            using_proxy: false,
            no_proxy,
            note: "出站代理: 直连（配置为 direct）".into(),
        },
        ProxySetting::Explicit(url) => match ureq::Proxy::new(url.as_str()) {
            Ok(p) => Egress {
                proxy: Some(p),
                using_proxy: true,
                no_proxy,
                note: format!("出站代理: {url}（配置为显式，始终使用）"),
            },
            Err(e) => {
                // The policy says "always proxy", so an unparseable URL is a
                // configuration error the user must see, not a reason to go
                // direct behind their back.
                crate::log::error(&format!(
                    "出站代理地址无法解析（{url}）: {e}；请修正 ccproxy.json 的 proxy 字段"
                ));
                Egress {
                    proxy: None,
                    using_proxy: false,
                    no_proxy,
                    note: format!("出站代理: 配置无效（{url}）— 地址无法解析，请修正配置"),
                }
            }
        },
        ProxySetting::Default => match system_proxy() {
            None => Egress {
                proxy: None,
                using_proxy: false,
                no_proxy,
                note: "出站代理: 直连（系统未配置代理）".into(),
            },
            Some(url) => match ureq::Proxy::new(url.as_str()) {
                Ok(p) if probe(&url) => Egress {
                    proxy: Some(p),
                    using_proxy: true,
                    no_proxy,
                    note: format!("出站代理: {url}（系统代理，探测通过）"),
                },
                Ok(_) => {
                    crate::log::warn(&format!(
                        "系统代理 {url} 探测失败；回退直连（proxy=\"default\" 允许回退，\
                         要强制使用请把 proxy 设为该地址）"
                    ));
                    Egress {
                        proxy: None,
                        using_proxy: false,
                        no_proxy,
                        note: format!("出站代理: 直连（系统代理 {url} 探测失败，已回退）"),
                    }
                }
                Err(e) => {
                    crate::log::warn(&format!("系统代理地址无法解析（{url}）: {e}；回退直连"));
                    Egress {
                        proxy: None,
                        using_proxy: false,
                        no_proxy,
                        note: "出站代理: 直连（系统代理地址无法解析）".into(),
                    }
                }
            },
        },
    }
}

/// The system proxy: `HTTPS_PROXY` first (a hand-started proxy follows its
/// shell), then the Windows registry — the same two sources the tray uses.
#[must_use]
pub fn system_proxy() -> Option<String> {
    for key in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(normalize(v));
            }
        }
    }
    registry_proxy()
}

fn normalize(url: &str) -> String {
    if url.contains("://") {
        url.to_string()
    } else {
        format!("http://{url}")
    }
}

#[cfg(windows)]
fn registry_proxy() -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    let enabled: u32 = key.get_value("ProxyEnable").ok()?;
    if enabled == 0 {
        return None;
    }
    let server: String = key.get_value("ProxyServer").ok()?;
    pick_https(&server)
}

#[cfg(not(windows))]
fn registry_proxy() -> Option<String> {
    None
}

/// Choose the `https` entry out of `https=a:1;http=b:2`, else `http`, else a
/// bare `host:port`.
///
/// Only the registry reader calls this, so on a platform without one the
/// function is compiled out — otherwise `-D warnings` fails non-Windows builds
/// with `pick_https is never used`. The unit test below covers the parsing
/// everywhere, which is why `test` keeps it alive too.
#[cfg(any(windows, test))]
fn pick_https(server: &str) -> Option<String> {
    let mut http = None;
    for part in server.split(';') {
        let Some((scheme, host)) = part.split_once('=') else {
            continue;
        };
        match scheme.trim() {
            "https" => return Some(format!("https://{}", host.trim())),
            "http" => http = Some(format!("http://{}", host.trim())),
            _ => {}
        }
    }
    let bare = server.trim();
    if !bare.is_empty() && !bare.contains('=') {
        return Some(normalize(bare));
    }
    http
}

/// Whether a request through `url` reaches upstream.
///
/// A real request, not a TCP connect: a proxy that accepts the socket but
/// cannot forward is unusable, and only a completed exchange proves otherwise.
/// Any HTTP response counts as success — the question is whether the proxy
/// carries traffic, not whether the request was valid — so only a transport
/// failure fails the probe.
fn probe(url: &str) -> bool {
    let Ok(proxy) = ureq::Proxy::new(url) else {
        return false;
    };
    let agent = ureq::AgentBuilder::new()
        .proxy(proxy)
        .timeout_connect(PROBE_TIMEOUT)
        .timeout_read(PROBE_TIMEOUT)
        .timeout_write(PROBE_TIMEOUT)
        .build();
    !matches!(agent.get(PROBE_URL).call(), Err(ureq::Error::Transport(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn egress_with(no_proxy: &[&str]) -> Egress {
        Egress {
            proxy: Some(ureq::Proxy::new("http://127.0.0.1:7897").unwrap()),
            using_proxy: true,
            no_proxy: no_proxy.iter().map(|s| (*s).to_string()).collect(),
            note: String::new(),
        }
    }

    #[test]
    fn host_is_extracted_from_every_url_shape() {
        assert_eq!(
            host_of("https://api.commandcode.ai/alpha/generate"),
            Some("api.commandcode.ai")
        );
        assert_eq!(host_of("http://127.0.0.1:8787/webui"), Some("127.0.0.1"));
        assert_eq!(
            host_of("https://user:pw@h.example:443/x"),
            Some("h.example")
        );
        assert_eq!(host_of("https://[::1]:8080/x"), Some("::1"));
        assert_eq!(host_of("api.example.com:80"), Some("api.example.com"));
    }

    #[test]
    fn loopback_always_bypasses_the_proxy() {
        // The proxy dials nothing on loopback today, but a config that routed
        // it through Clash would break the WebUI in a way that looks unrelated.
        let e = egress_with(&[]);
        assert!(e.bypasses("http://127.0.0.1:8787/webui"));
        assert!(e.bypasses("http://localhost:1/x"));
        assert!(e.bypasses("http://[::1]:2/x"));
        assert!(!e.bypasses("https://api.commandcode.ai/x"));
    }

    #[test]
    fn the_no_proxy_list_matches_hosts_and_subdomains() {
        let e = egress_with(&["example.com", ".internal"]);
        assert!(e.bypasses("https://example.com/x"));
        assert!(e.bypasses("https://a.example.com/x"));
        assert!(e.bypasses("https://host.internal/x"));
        // A suffix must fall on a label boundary, not mid-name.
        assert!(!e.bypasses("https://notexample.com/x"));
        assert!(!e.bypasses("https://example.com.evil.test/x"));
    }

    #[test]
    fn the_registry_proxy_list_prefers_https_then_http_then_a_bare_host() {
        // Windows stores per-scheme entries as `https=a:1;http=b:2`, and the
        // order in the string is the user's, not a priority — so an `http`
        // entry that comes first must still lose to a later `https` one.
        assert_eq!(
            pick_https("http=10.0.0.1:8080;https=10.0.0.1:8443"),
            Some("https://10.0.0.1:8443".to_string())
        );
        assert_eq!(
            pick_https("http=10.0.0.1:8080;ftp=10.0.0.1:21"),
            Some("http://10.0.0.1:8080".to_string())
        );
        // A bare `host:port` applies to every scheme.
        assert_eq!(
            pick_https("10.0.0.1:7897"),
            Some("http://10.0.0.1:7897".to_string())
        );
        // Nothing usable: a blank value, and a list of only other schemes.
        assert_eq!(pick_https("   "), None);
        assert_eq!(pick_https("ftp=10.0.0.1:21"), None);
    }
}
