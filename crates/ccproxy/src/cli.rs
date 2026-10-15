//! CLI subcommands: `start` / `stop` / `status` / `logs` / `config`.
//!
//! These are the Linux-first management surface: on a headless box there is no
//! tray, so the service is started, stopped and inspected from a shell (or from
//! systemd, which calls `start`). On Windows the tray keeps its role as the
//! primary supervisor; the same commands still work from a console.
//!
//! PID bookkeeping lives in the data directory (`CC_DATA_DIR`), overridable
//! with `CC_PID_FILE` for unusual layouts. systemd users ignore `stop` (the
//! unit manages the process); `status` still works from a shell.

use std::path::PathBuf;

use crate::config::Config;

/// Env override for the PID file location. The packaged layout points it at
/// the runtime dir when `CC_DATA_DIR` is unusual.
pub const PID_FILE_ENV: &str = "CC_PID_FILE";

/// The PID file: `<data dir>/ccproxy.pid`, or `CC_PID_FILE` when set.
#[must_use]
pub fn pid_file() -> PathBuf {
    std::env::var(PID_FILE_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::billing::billing_dir().join("ccproxy.pid"))
}

/// Read and parse the PID file. `None` when absent or unparseable.
#[must_use]
pub fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_file())
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
}

/// Write the current process id, creating the data directory as needed.
pub fn write_pid() -> std::io::Result<()> {
    let path = pid_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", std::process::id()))
}

/// Remove the PID file, ignoring a missing file.
pub fn remove_pid() {
    let _ = std::fs::remove_file(pid_file());
}

/// Ask one running instance to stop: `SIGTERM` on Unix (graceful — the server
/// drains and exits), `taskkill /T` on Windows. The target comes from the PID
/// file; without one there is nothing to stop.
fn send_terminate(pid: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()?;
    }
    #[cfg(windows)]
    {
        // Windows has no SIGTERM; taskkill's graceful mode is refused by
        // console processes, so this is the forceful `/T /F` variant. The
        // tree flag matters: the service may have spawned a child (the CLI
        // version lookup) that would otherwise keep the port alive.
        std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()?;
    }
    Ok(())
}

/// Probe `GET /health` on the configured listener. Returns the body on a 2xx,
/// or an error description. A short timeout keeps `status` snappy even when
/// nothing is listening.
fn health(host: &str, port: u16) -> Result<String, String> {
    let url = format!("http://{host}:{port}/health");
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(2))
        .build();
    match agent.get(&url).call() {
        Ok(resp) => resp.into_string().map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Print the effective configuration, with any proxy credentials masked.
pub fn describe_config(c: &Config) -> String {
    let proxy = match &c.proxy {
        crate::config::ProxySetting::Default => "default (system proxy)".to_string(),
        crate::config::ProxySetting::Direct => "direct".to_string(),
        crate::config::ProxySetting::Explicit(url) => mask_proxy_url(url),
    };
    format!(
        "host: {}\nport: {}\nupstream: {}\ncli version: {}\nlog level: {}\nproxy: {}\nno-proxy: {}\nwebui: {}\nlog file: {}\ndata dir: {}",
        c.host,
        c.port,
        c.cc_api_base,
        c.cc_version,
        c.log_level,
        proxy,
        if c.no_proxy.is_empty() {
            "(none)".to_string()
        } else {
            c.no_proxy.join(",")
        },
        crate::config::webui_site_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(not found)".to_string()),
        crate::config::log_path().display(),
        crate::billing::billing_dir().display(),
    )
}

/// Mask `user:pass@` in an explicit proxy URL: `http://user:***@host:port`.
fn mask_proxy_url(url: &str) -> String {
    let Some(rest) = url.split_once("://") else {
        return url.to_string();
    };
    let (scheme, authority) = (rest.0, rest.1);
    let Some((_, host)) = authority.rsplit_once('@') else {
        return url.to_string();
    };
    format!("{scheme}://***@{host}")
}

/// `stop`: signal the instance named by the PID file, wait for the port to
/// release, then remove the PID file. Returns a process exit code.
pub fn run_stop(c: &Config) -> i32 {
    let Some(pid) = read_pid() else {
        crate::log::error(&format!(
            "no PID file at {} — is the service running? (systemd-managed: use `systemctl stop ccproxy`)",
            pid_file().display()
        ));
        return 1;
    };
    crate::log::info(&format!("sending SIGTERM to pid {pid}"));
    if let Err(e) = send_terminate(pid) {
        crate::log::error(&format!("cannot signal pid {pid}: {e}"));
        return 1;
    }
    // Wait up to ~5s for the listener to go away before declaring success.
    #[allow(clippy::arithmetic_side_effects)]
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if health(&c.host, c.port).is_err() {
            remove_pid();
            crate::log::info("stopped");
            return 0;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    crate::log::warn("service did not release the port within 5s (PID file left in place)");
    1
}

/// `status`: PID file, process liveness via the /health probe, and the log
/// locations. Returns 0 when healthy, 1 otherwise.
pub fn run_status(c: &Config) -> i32 {
    let pid = read_pid();
    match &pid {
        Some(pid) => crate::log::info(&format!("pid: {pid} (from {})", pid_file().display())),
        None => crate::log::info(&format!(
            "no PID file at {} (service not started via CLI)",
            pid_file().display()
        )),
    }
    crate::log::info(&format!("listener: http://{}:{}", c.host, c.port));
    match health(&c.host, c.port) {
        Ok(body) => {
            crate::log::info(&format!("/health: {body}"));
            0
        }
        Err(e) => {
            crate::log::error(&format!("/health unreachable: {e}"));
            1
        }
    }
}

/// `logs`: print where the logs live and whether they exist.
pub fn run_logs() -> i32 {
    let proxy = crate::config::log_path();
    let tray = crate::config::tray_log_path();
    crate::log::info(&format!(
        "proxy log: {} ({})",
        proxy.display(),
        if proxy.is_file() { "present" } else { "absent" }
    ));
    crate::log::info(&format!(
        "tray log: {} ({})",
        tray.display(),
        if tray.is_file() { "present" } else { "absent" }
    ));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_urls_are_masked_at_the_authority_boundary() {
        assert_eq!(
            mask_proxy_url("http://user:secret@127.0.0.1:7897"),
            "http://***@127.0.0.1:7897"
        );
        // No credentials: unchanged.
        assert_eq!(
            mask_proxy_url("http://127.0.0.1:7897"),
            "http://127.0.0.1:7897"
        );
        // No scheme: unchanged.
        assert_eq!(mask_proxy_url("127.0.0.1:7897"), "127.0.0.1:7897");
    }

    #[test]
    fn pid_file_prefers_the_env_override() {
        // The decision is env-driven; the default path is derived from the
        // (test-affected) process environment, so only the env override is
        // asserted here.
        let dir = std::env::temp_dir();
        std::env::set_var(PID_FILE_ENV, dir.join("ccproxy-test.pid"));
        assert_eq!(pid_file(), dir.join("ccproxy-test.pid"));
        std::env::remove_var(PID_FILE_ENV);
    }

    #[test]
    fn describe_config_prints_an_explicit_proxy_masked() {
        let c = Config {
            proxy: crate::config::ProxySetting::Explicit("http://u:p@h:1".into()),
            ..Config::default()
        };
        let text = describe_config(&c);
        assert!(text.contains("http://***@h:1"), "{text}");
        assert!(!text.contains("u:p@"), "{text}");
    }
}
