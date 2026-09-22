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
    // The proxy keeps its own file so a hand-started run has a log too; the
    // tray's lifecycle lines go to a separate file, so the two never
    // interleave in one stream.
    ccproxy::log::init_file(&ccproxy::config::log_path());

    // Resolve the outbound proxy before anything dials out. This is where the
    // Rust build previously diverged from the Node one: it went direct while
    // the tray's HTTPS_PROXY was set, which surfaced as a flood of connection
    // timeouts against an upstream that was in fact reachable. The decision and
    // its reason are logged, so "did it use the proxy?" is answerable from the
    // log rather than inferred from the environment.
    let egress = ccproxy::proxy::init(&config);
    ccproxy::log::info(&egress.note);

    // CC's server blocks a stale `x-command-code-version`, so the published
    // version is consulted once before the listener opens. An explicit
    // CC_CLI_VERSION pins it and skips the lookup, so offline runs never wait
    // on DNS.
    let pinned = env("CC_CLI_VERSION");
    let before = config.cc_version.clone();
    config.cc_version = ccproxy::cli_version::resolve_cli_version(
        pinned.as_deref(),
        &config.cc_version,
        ccproxy::cli_version::fetch_latest_cli_version,
    );
    if config.cc_version != before {
        ccproxy::log::debug(&format!(
            "CLI version refreshed from npm: {}",
            config.cc_version
        ));
    }

    if config.port < 1024 {
        ccproxy::log::warn(&format!(
            "Port {} is in the privileged range (<1024); binding may require elevated privileges.",
            config.port
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

    let addr = format!("{}:{}", config.host, config.port);
    let server = match tiny_http::Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            let detail = format!("Failed to bind {addr}: {e}");
            ccproxy::log::error(&detail);
            report_startup_failure(&detail, config.port);
            std::process::exit(2);
        }
    };

    ccproxy::log::info(&format!(
        "cc-proxy listening on http://{addr} (upstream {})",
        config.cc_api_base
    ));

    ccproxy::server::serve(server, state);
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
