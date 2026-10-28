//! Entry point: load config, init logger, start the server.
//! Binary crate glue only — behaviour lives in the library modules.

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let env = |k: &str| std::env::var(k).ok();

    // A one-shot maintenance command rather than a server flag: importing the
    // legacy history is something you run once after upgrading, and it must not
    // be entangled with starting the listener.
    if argv.iter().any(|a| a == "--import-legacy-usage") {
        let mut config = ccproxy::config::load(&argv, &env);
        ccproxy::log::init(&config.log_level);
        config.log_level = "info".into();
        std::process::exit(import_legacy_usage(&config, &argv));
    }

    let mut config = ccproxy::config::load(&argv, &env);
    ccproxy::log::init(&config.log_level);

    // Subcommands: the first non-flag argument names the command; with none the
    // server starts (the pre-existing behaviour, so `ccproxy --port 9000`
    // keeps working and the tray's flag-less launch is unchanged).
    let command = argv
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(String::as_str)
        .unwrap_or("start");
    match command {
        "stop" => std::process::exit(ccproxy::cli::run_stop(&config)),
        "status" => std::process::exit(ccproxy::cli::run_status(&config)),
        "logs" => std::process::exit(ccproxy::cli::run_logs()),
        "config" => {
            println!("{}", ccproxy::cli::describe_config(&config));
            std::process::exit(0);
        }
        "start" => {}
        other => {
            ccproxy::log::error(&format!(
                "unknown command \"{other}\" (expected start | stop | status | logs | config)"
            ));
            std::process::exit(2);
        }
    }

    // The proxy keeps its own file so a hand-started run has a log too; the
    // tray's lifecycle lines go to a separate file, so the two never
    // interleave in one stream.
    ccproxy::log::init_file(&ccproxy::config::log_path());

    if config.port < 1024 {
        ccproxy::log::warn(&format!(
            "Port {} is in the privileged range (<1024); binding may require elevated privileges.",
            config.port
        ));
    }

    // ── bind first, then do the slow startup work ────────────────────
    //
    // The order here is a deployment contract, not a style choice. Everything
    // below — reading the system proxy out of the registry, the npm lookup for
    // the CLI version (up to 4 s of budget), building the request state — can
    // take seconds, and during a version handover those seconds are exactly
    // the window in which the previous process has already let go of the port.
    // Every client that dials in that window sees `ECONNREFUSED`: bili retries
    // for ~4.5 s and then returns 502 "acp-proxy failure", which is how a
    // 30-second manual window was diagnosed in the field.
    //
    // Binding first removes the window entirely. tiny_http starts accepting as
    // soon as the listener exists, so a request that arrives during the startup
    // work is queued and answered when serving begins, instead of being
    // refused. The wait is bounded by `STARTUP_LOOKUP_BUDGET`.
    let addr = format!("{}:{}", config.host, config.port);
    let server = match bind_with_retry(&addr) {
        Ok(s) => s,
        Err(detail) => {
            ccproxy::log::error(&detail);
            report_startup_failure(&detail, config.port);
            std::process::exit(2);
        }
    };
    ccproxy::log::info(&format!("listening socket bound on {addr}"));

    // Resolve the outbound proxy before anything dials out. This is where the
    // Rust build previously diverged from the Node one: it went direct while
    // the tray's HTTPS_PROXY was set, which surfaced as a flood of connection
    // timeouts against an upstream that was in fact reachable. The decision and
    // its reason are logged, so "did it use the proxy?" is answerable from the
    // log rather than inferred from the environment.
    let egress = ccproxy::proxy::init(&config);
    ccproxy::log::info(&egress.note);

    // CC's server blocks a stale `x-command-code-version`, so the published
    // version is consulted before serving — within a hard budget. The lookup
    // once took 35.8s (two 10s attempts on top of DNS resolution that ureq
    // does not bound) and blew the tray's 30s handover gate, so the budget is
    // the load-bearing wall: past it the fallback wins and the next startup
    // retries. An explicit CC_CLI_VERSION pins it and skips the lookup, so
    // offline runs never wait on DNS.
    let pinned = env("CC_CLI_VERSION");
    let before = config.cc_version.clone();
    config.cc_version = ccproxy::cli_version::resolve_within_budget(
        pinned.as_deref(),
        &config.cc_version,
        ccproxy::cli_version::fetch_latest_cli_version,
        ccproxy::cli_version::STARTUP_LOOKUP_BUDGET,
    );
    if config.cc_version != before {
        ccproxy::log::debug(&format!(
            "CLI version refreshed from npm: {}",
            config.cc_version
        ));
    }

    let state = ccproxy::server::new_state(config.clone());
    // The proxy is key-passthrough: the upstream key arrives per request, so
    // there is nothing to warm the catalog with at startup. The persisted
    // cache seeds the store instead, and model discovery stays the fallback
    // for names the cache does not know yet.
    if config.host != "127.0.0.1" && config.host != "localhost" && config.cors_origin == "*" {
        ccproxy::log::warn(&format!(
            "CORS is wide-open (\"*\") while HOST={} is exposed beyond localhost. \
             Set CORS_ORIGIN to a specific origin before exposing the proxy on a network.",
            config.host
        ));
    }

    ccproxy::log::info(&format!(
        "cc-proxy listening on http://{addr} (upstream {})",
        config.cc_api_base
    ));

    // The CLI's `stop` needs to know the instance: record the PID before
    // serving. A failure to write it is not fatal — the service still runs.
    if let Err(e) = ccproxy::cli::write_pid() {
        ccproxy::log::warn(&format!(
            "cannot write PID file at {}: {e} (stop via CLI will not work)",
            ccproxy::cli::pid_file().display()
        ));
    }

    // On Unix, SIGTERM/SIGINT are caught so `stop` (and `kill`/systemd stop)
    // shut the server down gracefully: the flag makes `serve_with_stop` return,
    // and the PID file is cleaned up below. No shutdown thread is needed —
    // tiny_http::Server is not Clone, so the serve loop polls the flag itself.
    #[cfg(unix)]
    let stop_flag = install_signal_stop();
    #[cfg(not(unix))]
    let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Silent update detection. It records "a newer release exists" in a state
    // file that the tray menu and the WebUI read; nothing is downloaded, shown,
    // or acted on without the user asking. Started here rather than in the
    // server so a stalled check can never delay the listener.
    ccproxy::update::spawn_detector();

    ccproxy::server::serve_with_stop(server, state, &stop_flag);

    #[cfg(unix)]
    if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
        ccproxy::log::info("shutdown signal received, stopping");
    }
    ccproxy::cli::remove_pid();
}

/// Bind the listening socket, retrying briefly while the address is still in
/// use.
///
/// A handover is `stop the old child` then `start the new one`. The old child's
/// listener is closed when the process exits, but the tray observes the exit
/// through `WaitForSingleObject`, and the kernel's socket teardown is not
/// guaranteed to be visible in the same instant. Without a retry, that
/// microsecond race becomes "the new version failed to start", which is
/// indistinguishable from a real conflict in the log.
///
/// Two seconds is long enough to outlast a teardown and short enough that a
/// genuine conflict (a foreign process holding the port) still fails fast —
/// and the failure is still reported, with the same file and message as
/// before.
fn bind_with_retry(addr: &str) -> Result<tiny_http::Server, String> {
    const WINDOW: std::time::Duration = std::time::Duration::from_millis(2_000);
    const STEP: std::time::Duration = std::time::Duration::from_millis(100);
    let started = std::time::Instant::now();
    loop {
        match tiny_http::Server::http(addr) {
            Ok(server) => {
                if started.elapsed() >= STEP {
                    ccproxy::log::info(&format!(
                        "bind succeeded after retry ({} ms) — the previous listener had not fully released the port",
                        started.elapsed().as_millis()
                    ));
                }
                return Ok(server);
            }
            Err(err) => {
                if !is_address_in_use(err.as_ref()) || started.elapsed() >= WINDOW {
                    return Err(format!("Failed to bind {addr}: {err}"));
                }
                std::thread::sleep(STEP);
            }
        }
    }
}

/// Whether a bind failure is the retryable "someone still holds this port".
fn is_address_in_use(err: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    err.downcast_ref::<std::io::Error>()
        .map(|e| e.kind() == std::io::ErrorKind::AddrInUse)
        .unwrap_or(false)
}

/// Catch SIGTERM/SIGINT into a flag that `serve_with_stop` polls.
#[cfg(unix)]
fn install_signal_stop() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    let flag = Arc::new(AtomicBool::new(false));
    let _ = signal_hook::flag::register(SIGTERM, Arc::clone(&flag));
    let _ = signal_hook::flag::register(SIGINT, Arc::clone(&flag));
    flag
}

/// Write a startup failure to a file beside the executable.
///
/// This binary is a console program: started from a shell it has somewhere to
/// print, but started by double-clicking — which is what happens to the wrong
/// executable in the package — the console closes with the process and the
/// message is gone. The first user of the packaged build hit exactly that and
/// read it as "no log, no error, broken release", so the failure is also left
/// on disk where it can be found afterwards.
fn report_startup_failure(detail: &str, port: u16) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let note = format!(
        "{detail}\n\n\
         This is the proxy service, not the program to start. Run CCProxyTray.exe,\n\
         which launches this binary and supervises it.\n\
         If port {port} is busy, another proxy (or a leftover from an earlier run)\n\
         already holds it.\n"
    );
    let _ = std::fs::write(dir.join("ccproxy-startup-error.txt"), note);
}

/// Import the pre-M7 `usage.jsonl` history into the ledger, then exit.
///
/// `--dir <path>` names a directory to search (repeatable, where the old builds
/// kept `logs/usage.jsonl`); with none given, the directories next to the
/// running executable and the release folder above it are scanned.
fn import_legacy_usage(config: &ccproxy::config::Config, argv: &[String]) -> i32 {
    let _ = config;
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    let mut iter = argv.iter();
    while let Some(arg) = iter.next() {
        if arg == "--dir" {
            if let Some(value) = iter.next() {
                dirs.push(std::path::PathBuf::from(value));
            }
        }
    }
    if dirs.is_empty() {
        dirs = legacy_search_dirs();
    }
    let dir = ccproxy::billing::billing_dir();
    let Some(conn) = ccproxy::billing::open_database(&dir) else {
        ccproxy::log::error(&format!("cannot open {}", dir.join("billing.db").display()));
        return 2;
    };
    let count = ccproxy::billing::import_legacy_jsonl(&conn, &dirs);
    if count == 0 {
        ccproxy::log::info(&format!(
            "[billing] nothing to import into {} (already imported, or no usage.jsonl in {} director(ies))",
            dir.join("billing.db").display(),
            dirs.len()
        ));
    }
    0
}

/// Version directories to scan for a legacy `usage.jsonl`.
fn legacy_search_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    // Beside the executable, and one level up: the release layout is
    // `CCProxy-Release/CCProxy-v0.4.4/dist/proxy.js`, with `logs/` next to it.
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(3) {
            dirs.push(ancestor.to_path_buf());
        }
    }
    // The upgrade layout keeps every version side by side.
    let release = std::path::Path::new("CCProxy-Release");
    if release.is_dir() {
        if let Ok(entries) = std::fs::read_dir(release) {
            for entry in entries.flatten() {
                dirs.push(entry.path());
            }
        }
    }
    dirs
}
