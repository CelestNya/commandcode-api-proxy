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

/// The file the proxy writes its own log to: `logs/proxy.log` beside the proxy
/// binary, namespaced for a test instance.
///
/// The tray's own `logs/tray.log` is a different stream — lifecycle lines only —
/// and opening that from the menu is how a user ends up staring at a two-line
/// file while the real errors sit next door under `service/`. This reuses the
/// proxy's own path function so the two can never drift apart again.
#[must_use]
pub fn proxy_log_path(root: &Path, ns: &str) -> PathBuf {
    let proxy_dir = proxy_binary(root)
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| root.to_path_buf());
    ccproxy::config::log_path_from(Some(&proxy_dir), Some(ns))
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

/// The settings file the *supervised proxy* reads, resolved from the proxy
/// binary rather than from the tray's own location.
///
/// The shipped layout puts the tray at `<root>/` and the proxy at
/// `<root>/service/`. Resolving from `current_exe` therefore named
/// `<root>/ccproxy.json` — a file the proxy never reads. The menu checkbox and
/// the toggle have been operating on that dead document: the checkbox said on
/// while the running proxy stayed off. `ccproxy::settings::path_for_proxy` is
/// the single answer both processes now agree on.
#[must_use]
pub fn proxy_config_path() -> Option<PathBuf> {
    let root = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let proxy = proxy_binary(&root)?;
    Some(ccproxy::settings::path_for_proxy(&proxy))
}

/// The image-compress state the menu should show, resolved the same way the
/// proxy resolves it (`CC_IMAGE_COMPRESS` env wins over ccproxy.json, both
/// missing means off).
#[must_use]
pub fn image_compress_enabled() -> bool {
    let env = std::env::var("CC_IMAGE_COMPRESS").ok();
    let file = proxy_config_path().map_or_else(ccproxy::config::FileConfig::default, |p| {
        ccproxy::config::load_file_config_at(&p)
    });
    ccproxy::config::image_compress_resolved(env.as_deref(), file.image_compress)
}

/// Toggle the image-compress switch by rewriting the proxy's ccproxy.json in
/// place, preserving every other key (including ones this build does not
/// model). Returns the new state.
///
/// The proxy reads its config once at startup, so the caller restarts it for
/// the change to apply; a stopped proxy simply picks the new value up on its
/// next start. The write goes through [`ccproxy::settings::Doc`], so it is
/// atomic — the restart can land the instant after.
pub fn toggle_image_compress(log: &crate::process::LogFile) -> bool {
    let new_state = !image_compress_enabled();
    let Some(path) = proxy_config_path() else {
        log.append("[tray] 图片压缩切换失败：无法定位代理旁的 ccproxy.json");
        return image_compress_enabled();
    };
    let mut doc = ccproxy::settings::Doc::load(&path);
    let patch = serde_json::json!({ "imageCompress": new_state });
    let applied = patch
        .as_object()
        .ok_or_else(|| "patch is not an object".to_string())
        .and_then(|map| doc.apply(map))
        .and_then(|_| doc.save().map_err(|e| e.to_string()));
    if let Err(err) = applied {
        log.append(&format!(
            "[tray] 图片压缩切换失败：写入 {} 出错（{err}）",
            path.display()
        ));
        return image_compress_enabled();
    }
    log.append(&format!(
        "[tray] 图片压缩 → {}（{}，重启代理生效）",
        if new_state { "开" } else { "关" },
        path.display()
    ));
    new_state
}

/// Move a pre-fix `<root>/ccproxy.json` to where the proxy actually reads it.
///
/// Before the paths were unified the tray wrote `<root>/ccproxy.json`. On an
/// install upgraded from that layout the proxy's own file still exists (it is
/// what the proxy booted from), so the legacy file is simply left behind —
/// deleting it would be the tidy move, but it is the only copy of a setting a
/// user toggled, so it is merged instead: keys the live file lacks are copied
/// in, keys it already has are left alone. Best effort; a failure leaves both
/// files as they were.
pub fn migrate_legacy_config(root: &Path, log: &crate::process::LogFile) {
    let legacy = root.join("ccproxy.json");
    let Some(target) = proxy_config_path() else {
        return;
    };
    if legacy == target {
        return; // a flat build: one file, nothing to merge
    }
    let Ok(text) = std::fs::read_to_string(&legacy) else {
        return;
    };
    let Ok(serde_json::Value::Object(old)) = serde_json::from_str(&text) else {
        return;
    };
    if old.is_empty() {
        return;
    }
    let mut doc = ccproxy::settings::Doc::load(&target);
    let mut copied = 0usize;
    for (key, value) in old {
        if doc.get(&key).is_none() && doc.apply_key(&key, value) {
            copied = copied.saturating_add(1);
        }
    }
    if copied == 0 {
        return;
    }
    match doc.save() {
        Ok(()) => log.append(&format!(
            "[tray] 已从旧位置合并 {copied} 项配置到 {}",
            target.display()
        )),
        Err(err) => log.append(&format!(
            "[tray] 旧配置合并失败（{}）：{err}",
            target.display()
        )),
    }
}

/// Repoint autostart at this install when it has drifted.///
/// The Run entry is only written when the user toggles autostart, so the
/// first handover that moves ownership to a new install directory leaves the
/// entry naming the old one — and the next reboot rolls the service back to
/// the old version. When this tray takes ownership it syncs: if autostart is
/// on and the registered command is a tray exe in some other directory, it is
/// rewritten to this exe. A command that is not a tray exe (a user's custom
/// wrapper) is left alone, and autostart being off means nothing to sync.
/// Adopt the previous install's `service/ccproxy.json` before starting the
/// proxy. Takeover and upgrades unpack a fresh package whose config carries
/// only zip defaults — notably no `imageCompress` — so without this a handover
/// silently changes what gets re-encoded, which shifts the request prefix and
/// drops the upstream prompt cache. The autostart entry names the incumbent
/// install (kept in sync since 0.7.2), so it doubles as the migration source.
/// Best effort in every direction: no registered command, same directory, or
/// unreadable/unparseable source config all leave the local file untouched.
pub fn inherit_config(root: &Path, log: &crate::process::LogFile) {
    if !namespace().is_empty() {
        return; // a test instance must not touch the production config
    }
    let Some(registered) = autostart_command() else {
        return; // autostart is off; no previous install is named anywhere
    };
    let old_exe = PathBuf::from(registered.trim().trim_matches('"'));
    let Some(old_root) = old_exe.parent() else {
        return;
    };
    if old_root == root {
        return; // same install restarting; nothing to inherit
    }
    let old_cfg = old_root.join("service").join("ccproxy.json");
    let Ok(text) = std::fs::read_to_string(&old_cfg) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        log.append(&format!(
            "[tray] 上一安装的配置无法解析（{old_cfg:?}），保留本安装配置"
        ));
        return;
    };
    let pretty = match serde_json::to_string_pretty(&value) {
        Ok(p) => p,
        Err(_) => return,
    };
    let new_cfg = root.join("service").join("ccproxy.json");
    if std::fs::write(&new_cfg, pretty.as_bytes()).is_ok() {
        log.append(&format!("[tray] 配置已继承自上一安装（{old_cfg:?}）"));
    } else {
        log.append("[tray] 配置继承写入失败（保留本安装配置）");
    }
}

pub fn sync_autostart(root: &Path, log: &crate::process::LogFile) {
    if !namespace().is_empty() {
        return; // a test instance must not touch the production autostart
    }
    let Some(registered) = autostart_command() else {
        return; // autostart is off; nothing can have drifted
    };
    let exe = std::env::current_exe().unwrap_or_else(|_| root.join("CCProxyTray.exe"));
    let expected = format!("\"{}\"", exe.display());
    if registered.trim() == expected {
        return;
    }
    if !is_tray_command(&registered) {
        log.append(&format!(
            "[tray] 自启动指向非托盘命令（{registered}），不擅自改动"
        ));
        return;
    }
    if crate::win::registry::write_string(RUN_KEY, RUN_VALUE, &expected) {
        log.append(&format!(
            "[tray] 自启动路径已同步到当前安装（原：{registered}）"
        ));
    } else {
        log.append("[tray] 自启动路径同步失败（保留原值）");
    }
}

/// The registered autostart command, decoded from `REG_SZ` bytes.
fn autostart_command() -> Option<String> {
    let bytes = crate::win::registry::read_bytes(RUN_KEY, RUN_VALUE)?;
    Some(reg_sz_to_string(&bytes))
}

/// Decode `REG_SZ` data: UTF-16LE code units, NUL-terminated, lenient on a
/// truncated tail.
#[must_use]
fn reg_sz_to_string(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks(2)
        .flat_map(|pair| match pair {
            [lo, hi] => Some(u16::from_le_bytes([*lo, *hi])),
            _ => None,
        })
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Whether the command line names a tray exe — i.e. something this code wrote.
/// A wrapper script or unrelated command is not ours to rewrite.
#[must_use]
fn is_tray_command(command: &str) -> bool {
    let path = command.trim().trim_matches('"');
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("CCProxyTray.exe"))
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

    /// A scratch directory per test: `set_image_compress_at` is the real write
    /// path, so the tests exercise `Doc` rather than a string transform.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ccproxy-tray-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The toggle's body, minus the menu plumbing: write `imageCompress` into
    /// `path` atomically, preserving every other key.
    fn set_image_compress_at(path: &Path, on: bool) -> bool {
        let mut doc = ccproxy::settings::Doc::load(path);
        let patch = serde_json::json!({ "imageCompress": on });
        let Some(map) = patch.as_object() else {
            return false;
        };
        doc.apply(map).is_ok() && doc.save().is_ok()
    }

    #[test]
    fn reg_sz_decodes_utf16le_up_to_the_nul() {
        let hello: Vec<u8> = "hello"
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(reg_sz_to_string(&hello), "hello");
        // An empty value (just the terminator) decodes to the empty string.
        assert_eq!(reg_sz_to_string(&[0, 0]), "");
        // A truncated tail is dropped, not panicked on.
        assert_eq!(reg_sz_to_string(&[0x68, 0x00]), "h");
        assert_eq!(reg_sz_to_string(&[0x68]), "");
    }

    #[test]
    fn tray_command_recognition_is_narrow() {
        assert!(is_tray_command(
            r#""D:\somewhere\CCProxy-Release\CCProxy-v0.7.2-rust\CCProxyTray.exe""#
        ));
        assert!(is_tray_command(r#""D:\somewhere\ccproxytray.exe""#));
        assert!(is_tray_command(r"D:\bare\CCProxyTray.exe"));
        // Not ours to rewrite:
        assert!(!is_tray_command(r#""D:\tools\my-wrapper.cmd" --flag"#));
        assert!(!is_tray_command(r#""C:\Program Files\App\app.exe""#));
        assert!(!is_tray_command(""));
    }

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

    #[test]
    fn the_proxy_log_sits_beside_the_proxy_binary_not_the_tray() {
        let dir = std::env::temp_dir().join("ccproxy-tray-logpath-test");
        let service = dir.join("service");
        std::fs::create_dir_all(&service).unwrap();
        std::fs::write(service.join("ccproxy.exe"), b"").unwrap();

        // The proxy lives in `service/`, so that is where its own resolver
        // puts `logs/proxy.log` — the tray must agree, or "打开日志" shows the
        // user an almost-empty file while the errors sit next door.
        assert_eq!(
            proxy_log_path(&dir, ""),
            service.join("logs").join("proxy.log")
        );
        assert_eq!(
            proxy_log_path(&dir, "abc"),
            service.join("logs").join("test-abc").join("proxy.log")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_toggle_preserves_every_other_config_key() {
        // The service config carries proxy/noProxy/maxBodyBytes; a toggle
        // that rewrote the file from a typed struct would silently drop keys
        // it does not model, so the edit must be value-level.
        let dir = temp_dir("toggle");
        let path = dir.join("ccproxy.json");
        std::fs::write(
            &path,
            r#"{
  "proxy": "default",
  "noProxy": "localhost,127.0.0.1",
  "maxBodyBytes": 33554432
}
"#,
        )
        .unwrap();
        set_image_compress_at(&path, true);
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            parsed.get("imageCompress"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(
            parsed.get("proxy"),
            Some(&serde_json::Value::String("default".into()))
        );
        assert_eq!(
            parsed.get("maxBodyBytes"),
            Some(&serde_json::Value::from(33_554_432u64))
        );
        // Toggling off flips the key back without losing anything either.
        set_image_compress_at(&path, false);
        let off: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            off.get("imageCompress"),
            Some(&serde_json::Value::Bool(false))
        );
        assert_eq!(off.get("noProxy"), parsed.get("noProxy"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_toggle_works_on_a_missing_or_corrupt_config() {
        // Fresh install: no file yet — the result is a valid one-key file.
        let dir = temp_dir("toggle-fresh");
        let path = dir.join("ccproxy.json");
        assert!(set_image_compress_at(&path, true));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&path).unwrap())
                .unwrap(),
            serde_json::json!({"imageCompress": true})
        );
        // Corrupt file: replaced wholesale rather than partially kept — a
        // merge with garbage would leave the proxy unable to parse it either.
        std::fs::write(&path, "{ not json").unwrap();
        assert!(set_image_compress_at(&path, false));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&path).unwrap())
                .unwrap(),
            serde_json::json!({"imageCompress": false})
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_legacy_root_config_is_merged_not_dropped() {
        // The pre-fix layout wrote `<root>/ccproxy.json`, which the proxy never
        // read. On upgrade the live file wins and only missing keys are copied
        // across — specifically for a key this build does not model.
        let dir = temp_dir("legacy");
        let root = dir.join("root");
        let service = root.join("service");
        std::fs::create_dir_all(&service).unwrap();
        std::fs::write(service.join("ccproxy.exe"), b"").unwrap();
        std::fs::write(root.join("ccproxy.json"), r#"{"imageCompress":true}"#).unwrap();
        std::fs::write(
            service.join("ccproxy.json"),
            r#"{"proxy":"direct","imageCompress":false}"#,
        )
        .unwrap();

        let target = service.join("ccproxy.json");
        assert_eq!(
            ccproxy::settings::path_for_proxy(&service.join("ccproxy.exe")),
            target
        );
        // The merge is keyed on the live file's contents, not on the legacy
        // file winning: imageCompress stays false because the live file has it.
        let legacy: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("ccproxy.json")).unwrap())
                .unwrap();
        let mut doc = ccproxy::settings::Doc::load(&target);
        let mut copied = 0usize;
        if let serde_json::Value::Object(map) = legacy {
            for (key, value) in map {
                if doc.get(&key).is_none() && doc.apply_key(&key, value) {
                    copied += 1;
                }
            }
        }
        doc.save().unwrap();
        let merged: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(copied, 0, "the live file already had every legacy key");
        assert_eq!(
            merged.get("imageCompress"),
            Some(&serde_json::Value::Bool(false))
        );
        assert_eq!(
            merged.get("proxy"),
            Some(&serde_json::Value::String("direct".into()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
