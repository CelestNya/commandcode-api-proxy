//! Everything the tray reads from its environment and the registry.
//!
//! Kept apart from the UI so the decisions are testable: the production port is
//! a contract and cannot be moved, the namespace isolates a test instance from
//! the running one, and the egress proxy follows the Windows system setting
//! unless `CC_PROXY` overrides it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The production port. A contract: never change it.
pub const PRODUCTION_PORT: u16 = 8787;

/// Instance namespace. Empty means the production instance.
#[must_use]
pub fn namespace() -> String {
    std::env::var("CC_TRAY_NS").unwrap_or_default()
}

/// Suffix a kernel-object name with the namespace, so an isolated instance gets
/// its own mutex and events instead of fighting the production tray for them.
#[must_use]
pub fn ns_name(base: &str) -> String {
    let ns = namespace();
    if ns.is_empty() {
        base.to_string()
    } else {
        format!("{base}-{ns}")
    }
}

/// The port this instance serves.
///
/// `CC_TRAY_PORT` is honoured **only** for a namespaced instance. Moving the
/// production port is a non-production act, and this asymmetry is what stops a
/// stray environment variable from relocating the real service.
#[must_use]
pub fn instance_port() -> u16 {
    if namespace().is_empty() {
        return PRODUCTION_PORT;
    }
    std::env::var("CC_TRAY_PORT")
        .ok()
        .and_then(|raw| raw.trim().parse::<u16>().ok())
        .filter(|p| *p > 0)
        .unwrap_or(PRODUCTION_PORT)
}

/// The version stamped into the package, read from the JSON beside the exe.
///
/// The proxy reports its own version over `/health`; this is only for the tray
/// tooltip and the folder name, so a missing file degrades to a placeholder
/// rather than an error.
#[must_use]
pub fn package_version(exe_dir: &Path) -> String {
    let candidates = [
        exe_dir.join("package.json"),
        exe_dir.join("..").join("package.json"),
    ];
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(v) = json.get("version").and_then(|v| v.as_str()) {
            return v.to_string();
        }
    }
    "unknown".into()
}

/// Where the rotating diagnostic log lives.
///
/// Under the exe (the shipped layout keeps `logs/` beside the proxy), with the
/// namespace split out so a test instance cannot pollute production logs.
#[must_use]
pub fn log_dir(root: &Path, ns: &str) -> PathBuf {
    let base = root.join("logs");
    if ns.is_empty() {
        base
    } else {
        base.join(format!("test-{ns}"))
    }
}

/// The outbound proxy to inject, following the Windows system setting.
///
/// `CC_PROXY=off` forces direct; any other non-empty value overrides. A
/// comma/semicolon list is reduced to the `https` entry, which is the one that
/// matters for reaching CC.
#[must_use]
pub fn resolve_proxy_url() -> Option<String> {
    let override_ = std::env::var("CC_PROXY").unwrap_or_default();
    if override_.eq_ignore_ascii_case("off") {
        return None;
    }
    if !override_.trim().is_empty() {
        return Some(normalize_proxy(override_.trim()));
    }
    system_proxy_from_registry()
}

fn normalize_proxy(url: &str) -> String {
    if url.contains("://") {
        url.to_string()
    } else {
        format!("http://{url}")
    }
}

/// Read `ProxyEnable` / `ProxyServer` from the current user's Internet Settings.
fn system_proxy_from_registry() -> Option<String> {
    // ProxyEnable is a REG_DWORD; only 1 counts.
    let enabled = reg_query_raw(INTERNET_SETTINGS, "ProxyEnable")?;
    if enabled.first().copied().unwrap_or(0) == 0 {
        return None;
    }
    let text = reg_query_string(INTERNET_SETTINGS, "ProxyServer")?;
    if let Some(best) = pick_https_from_list(&text) {
        return Some(best);
    }
    let trimmed = text.trim();
    if !trimmed.is_empty() && !trimmed.contains('=') {
        return Some(normalize_proxy(trimmed));
    }
    None
}

/// Choose the `https` (or else `http`) entry out of `https=a:1;http=b:2`.
fn pick_https_from_list(server: &str) -> Option<String> {
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
    http
}

/// The no-proxy list: loopback always, plus whatever the user configured.
///
/// `<local>` is a Windows-specific token meaning "no dot in the name"; it is
/// dropped because the tools consuming `NO_PROXY` do not understand it.
#[must_use]
pub fn build_no_proxy() -> String {
    let mut parts = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    if let Some(text) = reg_query_string(INTERNET_SETTINGS, "ProxyOverride") {
        for piece in text.split(';') {
            let piece = piece.trim();
            if !piece.is_empty() && piece != "<local>" && !parts.iter().any(|p| p == piece) {
                parts.push(piece.to_string());
            }
        }
    }
    parts.join(",")
}

/// The registry key holding the system proxy settings.
const INTERNET_SETTINGS: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

/// Read a registry value's raw bytes under HKCU. `None` when absent.
fn reg_query_raw(subkey: &str, value: &str) -> Option<Vec<u8>> {
    crate::win::registry::read_bytes(subkey, value)
}

/// Read a REG_SZ value as a string.
///
/// Registry strings are UTF-16, so decoding them as UTF-8 yields interleaved
/// NUL bytes rather than the text — which is why this goes through the
/// `u16`-aware path rather than `String::from_utf8_lossy`.
fn reg_query_string(subkey: &str, value: &str) -> Option<String> {
    crate::win::registry::read_string(subkey, value)
}

/// The proxy executable the tray launches.
///
/// `service/` is searched first: the shipped package keeps the proxy out of the
/// root so the one executable a person should double-click is unambiguous — the
/// first run of this package had both side by side, and picking the wrong one
/// looked exactly like a broken release. The root is still searched, because a
/// bare `cargo build` lays both binaries beside each other.
#[must_use]
pub fn proxy_binary(exe_dir: &Path) -> Option<PathBuf> {
    for dir in [exe_dir.join("service"), exe_dir.to_path_buf()] {
        for name in ["ccproxy.exe", "CCProxy.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Whether `pid`'s command line contains `needle` (case-insensitive).
///
/// Used to tell our Node proxy (`node dist\proxy.js`) from any other node on
/// the machine — the one thing the image name alone cannot answer.
#[must_use]
pub fn command_line_has(pid: u32, needle: &str) -> bool {
    let script = format!("(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').CommandLine");
    let Ok(out) = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
    else {
        return false;
    };
    let line = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    line.contains(&needle.to_ascii_lowercase())
}

/// `HKCU\...\Run` value name. One per namespace would be tidier, but a test
/// instance toggling autostart is not a thing that needs to survive reboot.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "CC Proxy Tray";

/// Whether the tray is registered to start with Windows.
#[must_use]
pub fn autostart_enabled() -> bool {
    crate::win::registry::read_bytes(RUN_KEY, RUN_VALUE).is_some()
}

/// Register or unregister autostart, logging which way it went.
pub fn toggle_autostart(root: &Path, log: &crate::process::LogFile) {
    if autostart_enabled() {
        if crate::win::registry::delete_value(RUN_KEY, RUN_VALUE) {
            log.append("[tray] 已关闭开机自启");
        } else {
            log.append("[tray] 关闭开机自启失败");
        }
        return;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| root.join("CCProxyTray.exe"));
    let quoted = format!("\"{}\"", exe.display());
    if crate::win::registry::write_string(RUN_KEY, RUN_VALUE, &quoted) {
        log.append(&format!("[tray] 已开启开机自启：{quoted}"));
    } else {
        log.append("[tray] 开启开机自启失败");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that read or write the process environment.
    ///
    /// Cargo runs tests on threads in one process, so `set_var` in one test is
    /// visible to every other — and `ns_name` reads `CC_TRAY_NS` live. Without
    /// this lock the namespace test intermittently saw an empty namespace when
    /// it raced the port test, and failed perhaps one run in three.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_production_port_cannot_be_moved_by_the_environment() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The namespace is empty in these tests, so CC_TRAY_PORT must be
        // ignored no matter what it says.
        // SAFETY: single-threaded test; restored below.
        unsafe { std::env::remove_var("CC_TRAY_NS") };
        // SAFETY: as above.
        unsafe { std::env::set_var("CC_TRAY_PORT", "9999") };
        assert_eq!(instance_port(), PRODUCTION_PORT);
        // SAFETY: as above.
        unsafe { std::env::remove_var("CC_TRAY_PORT") };
    }

    #[test]
    fn https_is_preferred_out_of_a_proxy_list() {
        assert_eq!(
            pick_https_from_list("http=1.2.3.4:80;https=5.6.7.8:443"),
            Some("https://5.6.7.8:443".into())
        );
        assert_eq!(
            pick_https_from_list("http=1.2.3.4:80"),
            Some("http://1.2.3.4:80".into())
        );
        assert_eq!(pick_https_from_list("nothing useful"), None);
    }

    #[test]
    fn a_bare_host_gets_a_scheme() {
        assert_eq!(normalize_proxy("1.2.3.4:8080"), "http://1.2.3.4:8080");
        assert_eq!(normalize_proxy("http://a:1"), "http://a:1");
    }

    #[test]
    fn the_namespace_suffixes_kernel_object_names() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: ENV_LOCK held; restored below.
        unsafe { std::env::remove_var("CC_TRAY_NS") };
        assert_eq!(ns_name("cc-proxy-tray"), "cc-proxy-tray");
        // SAFETY: as above.
        unsafe { std::env::set_var("CC_TRAY_NS", "test1") };
        assert_eq!(ns_name("cc-proxy-tray"), "cc-proxy-tray-test1");
        // SAFETY: as above.
        unsafe { std::env::remove_var("CC_TRAY_NS") };
    }

    #[test]
    fn a_test_namespace_gets_its_own_log_directory() {
        assert_eq!(
            log_dir(Path::new("C:\\pkg"), "abc"),
            Path::new("C:\\pkg").join("logs").join("test-abc")
        );
        assert_eq!(
            log_dir(Path::new("C:\\pkg"), ""),
            Path::new("C:\\pkg").join("logs")
        );
    }
}
