//! The settings API, end to end: a real listener, a real file on disk, and the
//! control file the tray consumes.
//!
//! This is the only place the write path is exercised through HTTP. The
//! module's own tests cover `Doc` and `control` in isolation; what they cannot
//! show is that the handler wires them together in the right order — read the
//! file, compare revisions, apply, save, then ask for a restart *with the new
//! revision*. Getting that order wrong is how "saved but not applied" becomes
//! a silent state nobody can see.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Serialises the tests in this file: `CC_PROXY_CONFIG` is process-wide.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A fresh directory holding one test's `ccproxy.json`.
fn fresh_config(tag: &str, seed: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "ccproxy-settings-api-{tag}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let config = dir.join("ccproxy.json");
    std::fs::write(&config, seed).expect("seed config");
    // SAFETY: the lock is held for the whole test, and this binary is one
    // process (cargo gives each integration test file its own).
    std::env::set_var(ccproxy::settings::CONFIG_ENV, &config);
    (dir, config)
}

fn config() -> ccproxy::config::Config {
    ccproxy::config::Config {
        host: "127.0.0.1".into(),
        port: 0,
        cc_api_base: "http://127.0.0.1:1".into(),
        cc_version: "1.0.0".into(),
        log_level: "error".into(),
        cors_origin: "*".into(),
        upstream_timeout_ms: 1000,
        idle_timeout_ms: 1000,
        no_output_timeout_ms: 0,
        no_output_retries: 0,
        max_body_bytes: 1024 * 1024,
        ..ccproxy::config::Config::default()
    }
}

fn start() -> u16 {
    let state = ccproxy::server::new_state(config());
    let (port, _handle) = ccproxy::server::serve_on_ephemeral_port(state).expect("listen");
    port
}

fn send(port: u16, method: &str, path: &str, body: Option<&Value>) -> (u16, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let payload = body.map(ToString::to_string).unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    socket.write_all(request.as_bytes()).expect("write");
    let mut raw = Vec::new();
    let _ = socket.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

fn json_of(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

fn read_config(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("json")
}

#[test]
fn the_settings_api_reports_the_document_and_its_field_table() {
    let _guard = lock();
    let (dir, _config_path) = fresh_config("get", r#"{"proxy":"direct","imageCompress":true}"#);
    let port = start();

    let (status, body) = send(port, "GET", "/webui/api/settings", None);
    assert_eq!(status, 200, "{body}");
    let doc = json_of(&body);
    assert_eq!(doc["values"]["proxy"], json!("direct"));
    assert_eq!(doc["values"]["imageCompress"], json!(true));
    // A field the file does not mention reports its default, so the page never
    // has to guess.
    assert_eq!(doc["values"]["imageCompressMinBytes"], json!(512_000));
    assert_eq!(doc["runningVersion"], json!(ccproxy::proxy_version()));
    let fields = doc["fields"].as_array().expect("field table");
    assert!(fields.iter().any(|f| f["key"] == json!("imageCompress")));
    // Every field says whether it needs a restart, because the answer is not
    // uniform for a future field that is read per request.
    assert!(fields.iter().all(|f| f["restart"] == json!(true)));
    assert_eq!(doc["corrupt"], json!(false));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn saving_a_change_writes_the_file_and_asks_for_a_restart() {
    let _guard = lock();
    let (dir, config_path) = fresh_config(
        "save",
        r#"{"proxy":"default","maxBodyBytes":33554432,"imageCompress":false}"#,
    );
    let port = start();

    let (status, body) = send(port, "GET", "/webui/api/settings", None);
    assert_eq!(status, 200);
    let revision = json_of(&body)["revision"]
        .as_str()
        .expect("revision")
        .to_string();

    let patch = json!({
        "baseRevision": revision,
        "values": {"imageCompress": true, "imageCompressQuality": 70},
        "restart": true,
    });
    let (status, body) = send(port, "POST", "/webui/api/settings", Some(&patch));
    assert_eq!(status, 200, "{body}");
    let reply = json_of(&body);
    assert_eq!(reply["ok"], json!(true));
    assert_eq!(
        reply["changed"],
        json!(["imageCompress", "imageCompressQuality"])
    );
    assert_eq!(reply["restartRequested"], json!(true));

    let on_disk = read_config(&config_path);
    assert_eq!(on_disk["imageCompress"], json!(true));
    assert_eq!(on_disk["imageCompressQuality"], json!(70));
    assert_eq!(
        on_disk["maxBodyBytes"],
        json!(33_554_432),
        "keys this build does not edit must survive an edit of the ones it does"
    );
    assert_eq!(
        on_disk["proxy"],
        json!("default"),
        "the untouched egress setting survives too"
    );

    // The restart request carries the revision that was just written, which is
    // what lets the tray log say what it is picking up.
    let control = std::fs::read_to_string(dir.join(".ccproxy-control.json")).expect("control file");
    let control = json_of(&control);
    assert_eq!(control["restart"]["reason"], json!("settings"));
    assert_eq!(control["restart"]["revision"], reply["revision"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_save_from_a_stale_page_is_refused_with_the_current_document() {
    let _guard = lock();
    let (dir, config_path) = fresh_config("conflict", r#"{"logLevel":"info"}"#);
    let port = start();

    // The page loaded this revision...
    let (_, body) = send(port, "GET", "/webui/api/settings", None);
    let stale = json_of(&body)["revision"]
        .as_str()
        .expect("revision")
        .to_string();

    // ...then the file changed underneath it (a tray toggle, another tab).
    std::fs::write(&config_path, r#"{"logLevel":"debug"}"#).expect("edit");

    let patch = json!({
        "baseRevision": stale,
        "values": {"imageCompress": true},
    });
    let (status, body) = send(port, "POST", "/webui/api/settings", Some(&patch));
    assert_eq!(status, 409, "a stale write must not win: {body}");
    let reply = json_of(&body);
    assert_eq!(reply["conflict"], json!(true));
    assert_eq!(
        reply["document"]["values"]["logLevel"],
        json!("debug"),
        "the reply carries the current document so the page can re-render"
    );
    assert_eq!(
        read_config(&config_path)["imageCompress"],
        Value::Null,
        "the refused patch left no trace"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_out_of_range_value_is_rejected_by_name_and_nothing_is_written() {
    let _guard = lock();
    let (dir, config_path) = fresh_config("invalid", r#"{"imageCompressQuality":80}"#);
    let port = start();

    let patch = json!({"values": {"imageCompressQuality": 500, "logLevel": "debug"}});
    let (status, body) = send(port, "POST", "/webui/api/settings", Some(&patch));
    assert_eq!(status, 400, "{body}");
    assert!(
        json_of(&body)["error"]
            .as_str()
            .unwrap_or_default()
            .contains("压缩质量"),
        "the message names the offending field: {body}"
    );
    let on_disk = read_config(&config_path);
    assert_eq!(
        on_disk["logLevel"],
        Value::Null,
        "the valid half of the patch must not be applied alone"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_restart_can_be_requested_without_touching_the_settings() {
    let _guard = lock();
    let (dir, _config_path) = fresh_config("restart", r#"{"proxy":"direct"}"#);
    let port = start();

    let (status, body) = send(port, "POST", "/webui/api/restart", Some(&json!({})));
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_of(&body)["requested"], json!(true));
    let control =
        json_of(&std::fs::read_to_string(dir.join(".ccproxy-control.json")).expect("control file"));
    assert_eq!(control["restart"]["reason"], json!("webui"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_update_endpoint_reports_the_running_version_even_when_never_checked() {
    let _guard = lock();
    let (dir, _config_path) = fresh_config("update", r#"{"proxy":"direct"}"#);
    let port = start();

    let (status, body) = send(port, "GET", "/webui/api/update", None);
    assert_eq!(status, 200, "{body}");
    let reply = json_of(&body);
    assert_eq!(reply["update"]["current"], json!(ccproxy::proxy_version()));
    assert_eq!(reply["update"]["available"], json!(false));
    assert_eq!(reply["update"]["checkedAt"], Value::Null);
    assert_eq!(
        reply["update"]["installable"],
        json!(false),
        "nothing to install before a check has run"
    );
    assert_eq!(reply["job"]["phase"], json!(""));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn installing_nothing_is_refused_rather_than_starting_a_pointless_job() {
    let _guard = lock();
    let (dir, _config_path) = fresh_config("install-none", r#"{"proxy":"direct"}"#);
    let port = start();

    let (status, body) = send(
        port,
        "POST",
        "/webui/api/update",
        Some(&json!({"action": "install"})),
    );
    assert_eq!(status, 400, "{body}");
    assert!(json_of(&body)["error"]
        .as_str()
        .unwrap_or_default()
        .contains("没有可安装"));
    let _ = std::fs::remove_dir_all(&dir);
}
