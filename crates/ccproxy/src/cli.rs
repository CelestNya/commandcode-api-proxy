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

/// The usage text for `--help`.
///
/// Kept next to the command dispatch so a new subcommand is added in one file.
pub const HELP: &str = "\
cc-proxy — local proxy for the Command Code API

USAGE:
    ccproxy [COMMAND] [OPTIONS]

COMMANDS:
    start               Run the proxy (the default when no command is given)
    stop                Signal the instance named by the PID file, then wait
                        for the port to be released
    status              Print the PID file and probe /health
    logs                Print where the proxy and tray logs live
    config              Print the effective configuration (credentials masked)

OPTIONS:
    --port <N>          Listen port (default 8787)
    --host <H>          Bind address (default 127.0.0.1)
    --import-legacy-usage [--dir <PATH>]...
                        Import a pre-M7 usage.jsonl into the ledger, then exit
    -h, --help          Print this help and exit
    -V, --version       Print the version and exit

ENVIRONMENT (see docs for the full list):
    PORT, HOST, CC_API_BASE, CC_DATA_DIR, CC_LOG_DIR, CC_WEBUI_DIR,
    CC_PROXY, NO_PROXY, CC_UPSTREAM_TIMEOUT_MS, CC_IDLE_TIMEOUT_MS,
    CC_NO_OUTPUT_TIMEOUT_MS, CC_NO_OUTPUT_RETRIES, CC_PID_FILE
";

/// Flags whose next argument is their value.
///
/// Only the two the config loader actually reads. Everything else is accepted
/// and ignored (matching the Node parser), so a positional argument after an
/// unknown flag is still a command name.
const VALUE_FLAGS: [&str; 2] = ["--port", "--host"];

/// The subcommand name, or `None` for a flag-only invocation.
///
/// The first positional argument names the command — but a flag's *value* is
/// positional too. Reading them the same way made `ccproxy --port 8803` fail
/// with `unknown command "8803"`, even though that form is documented and is
/// how the flag-only launch has always been spelled.
#[must_use]
pub fn subcommand(argv: &[String]) -> Option<&str> {
    let mut iter = argv.iter();
    while let Some(arg) = iter.next() {
        if VALUE_FLAGS.contains(&arg.as_str()) {
            // Consume the value unconditionally: a missing value is the config
            // loader's problem, not a command name.
            iter.next();
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg);
    }
    None
}

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
///
/// The child's own output is captured and dropped rather than inherited. Two
/// reasons, both observed: on a zh-CN Windows `taskkill` writes its message in
/// the OEM code page, which lands in a UTF-8 console as mojibake; and the
/// wording is localised, so matching on it (to tell "already gone" from "access
/// denied") cannot work. The exit status is the only portable signal, and the
/// caller confirms the outcome with the `/health` probe anyway.
fn send_terminate(pid: u32) -> Result<(), String> {
    #[cfg(unix)]
    let mut command = {
        let mut c = std::process::Command::new("kill");
        c.arg("-TERM").arg(pid.to_string());
        c
    };
    #[cfg(windows)]
    let mut command = {
        // Windows has no SIGTERM; taskkill's graceful mode is refused by
        // console processes, so this is the forceful `/T /F` variant. The
        // tree flag matters: the service may have spawned a child (the CLI
        // version lookup) that would otherwise keep the port alive.
        let mut c = std::process::Command::new("taskkill");
        c.args(["/PID", &pid.to_string(), "/T", "/F"]);
        c
    };

    let output = command
        .output()
        .map_err(|e| format!("cannot run the stop command: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(match output.status.code() {
        Some(code) => format!("stop command exited with status {code}"),
        None => "stop command was terminated by a signal".to_string(),
    })
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
    // A failed signal is not reported as a failed stop until the port has been
    // probed: the usual cause is that the process is already gone (a stale PID
    // file, or the tray stopped it first), and saying "cannot signal" then exit
    // 1 reads as a bug when nothing is wrong. A real failure — the port still
    // answering — is reported below, where it can be stated as such.
    if let Err(e) = send_terminate(pid) {
        crate::log::warn(&format!("{e}; checking whether the service is still up"));
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
    crate::log::error(&format!(
        "pid {pid} did not stop within 5s — something is still listening on {}:{} (PID file left in place)",
        c.host, c.port
    ));
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
    fn a_flag_value_is_not_a_command() {
        let args = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        // The documented flag-only forms: no command, so the server starts.
        let flag_only = args(&["--port", "8803"]);
        assert_eq!(subcommand(&flag_only), None);
        let two_flags = args(&["--host", "0.0.0.0", "--port", "9000"]);
        assert_eq!(subcommand(&two_flags), None);
        let nothing = args(&[]);
        assert_eq!(subcommand(&nothing), None);
        // A value that happens to look like a command is still a value.
        let tricky = args(&["--port", "config"]);
        assert_eq!(subcommand(&tricky), None);

        // The command is found wherever it sits, before or after the flags.
        let leading = args(&["start", "--port", "9000"]);
        assert_eq!(subcommand(&leading), Some("start"));
        let trailing = args(&["--port", "9000", "status"]);
        assert_eq!(subcommand(&trailing), Some("status"));
    }

    #[test]
    fn the_help_text_names_every_command_that_can_be_run() {
        for command in ["start", "stop", "status", "logs", "config"] {
            assert!(HELP.contains(command), "help omits `{command}`");
        }
    }

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
