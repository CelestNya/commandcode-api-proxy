//! Entry point: load config, init logger, start the server.
//! Binary crate glue only — behaviour lives in the library modules.

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let env = |k: &str| std::env::var(k).ok();
    let config = ccproxy::config::load(&argv, &env);

    ccproxy::log::init(&config.log_level);

    if config.port < 1024 {
        ccproxy::log::warn(&format!(
            "Port {} is in the privileged range (<1024); binding may require elevated privileges.",
            config.port
        ));
    }

    let state = ccproxy::server::new_state(config.clone());
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
            ccproxy::log::error(&format!("Failed to bind {addr}: {e}"));
            std::process::exit(2);
        }
    };

    ccproxy::log::info(&format!(
        "cc-proxy listening on http://{addr} (upstream {})",
        config.cc_api_base
    ));

    ccproxy::server::serve(server, state);
}
