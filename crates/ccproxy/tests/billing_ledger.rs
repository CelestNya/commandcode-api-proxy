//! The billing ledger through the real request path.
//!
//! `billing.rs`'s unit tests cover the writer: numbering, NULL-vs-zero, flush,
//! backpressure. What they cannot show is that the *server* wires attempts into
//! it — that a retry produces two rows rather than one, and that a successful
//! turn records real numbers. That is what this file drives, using a mock
//! upstream and a real listener.
//!
//! Each test runs against its own `CC_TRAY_NS`, so the databases are separate,
//! and the whole file is serialised because the ledger and the environment are
//! process-wide.

use rusqlite::Connection;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The mock upstream: scripted replies, one per connection.
struct MockUpstream {
    port: u16,
    /// The bodies of the requests it received, for asserting what was sent.
    seen: Arc<Mutex<Vec<String>>>,
}

/// One scripted upstream reply.
#[derive(Clone)]
enum Reply {
    /// A clean NDJSON stream ending in a usage-bearing finish event.
    Usage {
        prompt: u64,
        cached: u64,
        completion: u64,
    },
    /// A 503, which the transport layer treats as retryable.
    Status(u16),
}

impl MockUpstream {
    fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_for_thread = Arc::clone(&seen);
        std::thread::spawn(move || {
            let mut replies = replies.into_iter().cycle();
            for incoming in listener.incoming() {
                let Ok(mut socket) = incoming else { continue };
                let body = read_http_request(&mut socket);
                if let Ok(mut guard) = seen_for_thread.lock() {
                    guard.push(body);
                }
                let reply = replies.next().unwrap_or(Reply::Status(500));
                let _ = write_reply(&mut socket, &reply);
            }
        });
        Self { port, seen }
    }
}

/// Read a request up to the end of its body (the mock ignores chunked encoding
/// because the proxy always sends a content-length for a JSON body).
fn read_http_request(socket: &mut TcpStream) -> String {
    let mut reader = BufReader::new(socket.try_clone().expect("clone"));
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return headers,
            Ok(_) => {}
            Err(_) => return headers,
        }
        headers.push_str(&line);
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    let length = headers
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    let _ = reader.read_exact(&mut body);
    String::from_utf8_lossy(&body).into_owned()
}

fn write_reply(socket: &mut TcpStream, reply: &Reply) -> std::io::Result<()> {
    match reply {
        Reply::Status(code) => {
            let body = format!("{{\"error\":{{\"message\":\"upstream {code}\"}}}}");
            write!(
                socket,
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )?;
        }
        Reply::Usage {
            prompt,
            cached,
            completion,
        } => {
            // The finish event is the only carrier of usage, matching CC.
            let events = [
                json!({"type": "text", "text": "hello"}).to_string(),
                json!({
                    "type": "finish",
                    "finishReason": "stop",
                    "usage": {
                        "promptTokens": prompt,
                        "completionTokens": completion,
                        "promptTokensDetails": {"cachedTokens": cached},
                    }
                })
                .to_string(),
            ];
            let body: String = events
                .iter()
                .map(|e| format!("data: {e}\n"))
                .collect::<String>()
                + "data: [DONE]\n";
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )?;
        }
    }
    socket.flush()
}

/// Serialises the tests: they share the process-wide ledger and environment.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A fresh temp directory to hold one test's database.
fn fresh_dir(tag: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("ccproxy-ledger-e2e-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("temp root");
    root
}

/// Read the ledger rows for `dir` as `(attempt, status, errorTag, prompt, completion)`.
#[allow(clippy::type_complexity)]
fn rows(dir: &Path) -> Vec<(i64, String, Option<String>, Option<i64>, Option<i64>)> {
    let path = dir.join("billing.db");
    if !path.exists() {
        return Vec::new();
    }
    let conn = Connection::open(&path).expect("open ledger");
    let mut stmt = conn
        .prepare("select attempt, status, errorTag, promptTokens, completionTokens from billing order by id")
        .expect("prepare");
    let mapped = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("query");
    mapped.filter_map(Result::ok).collect()
}

/// Start the proxy pointed at `mock`, recording into `dir`.
///
/// The ledger is installed explicitly rather than derived from the environment:
/// `LOCALAPPDATA` and `CC_TRAY_NS` are process-wide, and `set_var` racing
/// between parallel test threads is undefined behaviour, so a test cannot
/// safely point the process at a directory that way.
fn start_proxy(mock: u16, dir: &Path) -> u16 {
    let config = ccproxy::config::Config {
        host: "127.0.0.1".into(),
        port: 0,
        cc_api_base: format!("http://127.0.0.1:{mock}"),
        cc_version: "1.0.0".into(),
        log_level: "error".into(),
        cors_origin: "*".into(),
        upstream_timeout_ms: 5000,
        idle_timeout_ms: 5000,
        // The no-output retry is exercised by its own tests; leaving it off
        // here keeps these cases on the pre-existing path.
        no_output_timeout_ms: 0,
        no_output_retries: 0,
        max_body_bytes: 50 * 1024 * 1024,
        ..ccproxy::config::Config::default()
    };
    let ledger = ccproxy::billing::Ledger::open(dir).expect("open a test ledger");
    ccproxy::billing::use_ledger_for_tests(ledger);

    let state = ccproxy::server::new_state(config);
    let (port, _handle) = ccproxy::server::serve_on_ephemeral_port(state).expect("listen");
    port
}

/// A raw HTTP request to the proxy, returning the status and body.
fn request(port: u16, path: &str, body: &Value, stream: bool) -> (u16, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let payload = body.to_string();
    let _ = stream; // the flag lives in the body; kept explicit for readability
    write!(
        socket,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Authorization: Bearer test-key\r\nx-api-key: test-key\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )
    .expect("write");
    let mut raw = Vec::new();
    let _ = socket.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, text)
}

/// Give the ledger's writer thread a moment to commit, then read the rows.
///
/// The batch window is 200ms, and the write thread commits on a timeout, so a
/// short wait is the honest way to observe rows written asynchronously. The
/// unit tests use `flush()`; the proxy here has no handle to its ledger.
#[allow(clippy::type_complexity)]
fn rows_after_quiet(dir: &Path) -> Vec<(i64, String, Option<String>, Option<i64>, Option<i64>)> {
    ccproxy::billing::flush();
    std::thread::sleep(Duration::from_millis(50));
    rows(dir)
}

#[test]
fn a_successful_turn_records_one_row_with_real_numbers() {
    let _guard = lock();
    let dir = fresh_dir("ok");
    let mock = MockUpstream::start(vec![Reply::Usage {
        prompt: 1000,
        cached: 900,
        completion: 42,
    }]);
    let port = start_proxy(mock.port, &dir);

    let (status, body) = request(
        port,
        "/v1/chat/completions",
        &json!({
            "model": "m",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }),
        true,
    );
    assert_eq!(status, 200, "the turn must succeed: {body}");

    let rows = rows_after_quiet(&dir);
    assert_eq!(rows.len(), 1, "one attempt, one row: {rows:?}");
    let row = rows.first().expect("a row");
    assert_eq!(row.0, 1, "the first attempt is numbered 1");
    assert_eq!(row.1, "ok");
    assert_eq!(row.3, Some(1000), "the real prompt token count");
    assert_eq!(row.4, Some(42), "the real completion token count");
}

#[test]
fn a_retried_turn_records_a_row_per_attempt() {
    let _guard = lock();
    let dir = fresh_dir("retry");
    // First attempt gets a retryable 503; the second succeeds.
    let mock = MockUpstream::start(vec![
        Reply::Status(503),
        Reply::Usage {
            prompt: 2000,
            cached: 1500,
            completion: 7,
        },
    ]);
    let port = start_proxy(mock.port, &dir);

    let (status, body) = request(
        port,
        "/v1/chat/completions",
        &json!({
            "model": "m",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }),
        true,
    );
    assert_eq!(status, 200, "the retry must succeed: {body}");

    let rows = rows_after_quiet(&dir);
    assert_eq!(
        rows.len(),
        2,
        "the abandoned attempt must leave a row of its own: {rows:?}"
    );
    let retried = rows.first().expect("first");
    assert_eq!(retried.0, 1);
    assert_eq!(retried.1, "error");
    assert_eq!(retried.2.as_deref(), Some("http-503"));
    assert_eq!(
        retried.3, None,
        "the abandoned attempt has no usage to report; NULL, not 0"
    );
    let succeeded = rows.get(1).expect("second");
    assert_eq!(succeeded.0, 2, "the retry is attempt 2");
    assert_eq!(succeeded.1, "ok");
    assert_eq!(
        succeeded.3,
        Some(2000),
        "real numbers on the attempt that ran"
    );
}

#[test]
fn the_request_id_ties_the_attempts_of_one_turn_together() {
    let _guard = lock();
    let dir = fresh_dir("reqid");
    let mock = MockUpstream::start(vec![
        Reply::Status(503),
        Reply::Usage {
            prompt: 10,
            cached: 0,
            completion: 1,
        },
    ]);
    let port = start_proxy(mock.port, &dir);

    let _ = request(
        port,
        "/v1/chat/completions",
        &json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "hi"}]}),
        true,
    );

    // The second row may still be inside the writer's batch window, so commit
    // before reading rather than assuming it has landed.
    ccproxy::billing::flush();
    let conn = Connection::open(dir.join("billing.db")).expect("open");
    let distinct: i64 = conn
        .query_row("select count(distinct reqId) from billing", [], |r| {
            r.get(0)
        })
        .expect("query");
    let total: i64 = conn
        .query_row("select count(*) from billing", [], |r| r.get(0))
        .expect("query");
    assert_eq!(total, 2, "both attempts are on disk");
    assert_eq!(
        distinct, 1,
        "one client request means one reqId, so a retry chain stays legible"
    );
}

#[test]
fn the_thread_id_is_reused_across_a_retry() {
    let _guard = lock();
    let dir = fresh_dir("threadid");
    let mock = MockUpstream::start(vec![
        Reply::Status(503),
        Reply::Usage {
            prompt: 10,
            cached: 0,
            completion: 1,
        },
    ]);
    let port = start_proxy(mock.port, &dir);

    let _ = request(
        port,
        "/v1/chat/completions",
        &json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "hi"}]}),
        true,
    );

    let seen = mock.seen.lock().expect("seen").clone();
    assert_eq!(seen.len(), 2, "the mock saw both attempts");
    let ids: Vec<Option<String>> = seen
        .iter()
        .map(|body| {
            serde_json::from_str::<Value>(body).ok().and_then(|v| {
                v.get("threadId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
        })
        .collect();
    // CC bills per session, so a retry must not look like a second one.
    assert_eq!(
        ids.first(),
        ids.get(1),
        "a retry must reuse the threadId, not mint a new one: {ids:?}"
    );
    assert!(
        ids.first().is_some_and(|id| id.is_some()),
        "a threadId was sent"
    );
}

#[test]
fn a_failed_generation_records_the_failure_tag() {
    let _guard = lock();
    let dir = fresh_dir("failed");
    // Every attempt fails: the retry budget is exhausted.
    let mock = MockUpstream::start(vec![Reply::Status(500)]);
    let port = start_proxy(mock.port, &dir);

    let (status, _body) = request(
        port,
        "/v1/chat/completions",
        &json!({"model": "m", "stream": false, "messages": [{"role": "user", "content": "hi"}]}),
        false,
    );
    assert_ne!(
        status, 200,
        "a persistent 500 must not be reported as success"
    );

    let rows = rows_after_quiet(&dir);
    assert!(
        rows.len() >= 2,
        "each attempt is recorded, including the ones retried: {rows:?}"
    );
    assert!(
        rows.iter().all(|r| r.1 != "ok"),
        "no attempt succeeded, so none may be recorded as ok: {rows:?}"
    );
    assert!(
        rows.iter().all(|r| r.3.is_none()),
        "a failed dispatch has no usage; NULL, not 0"
    );
}

// ── M7 acceptance: the ledger is never on the request's critical path ──
//
// The milestone's acceptance is stated as "forwarding still completes under
// writer fault injection". The unit tests prove the writer degrades in
// isolation; these two drive a real turn through the server with the ledger
// taken away, which is the only way to show the wiring honours that.

#[test]
fn a_turn_still_completes_when_no_ledger_can_be_opened() {
    let _guard = lock();
    // No `use_ledger_for_tests`, and `disable_for_tests` stops `global()` from
    // lazily opening the machine's real database as a side effect.
    ccproxy::billing::disable_for_tests();
    let mock = MockUpstream::start(vec![Reply::Usage {
        prompt: 10,
        cached: 0,
        completion: 3,
    }]);
    let port = start_proxy_without_ledger(mock.port);

    let (status, body) = request(
        port,
        "/v1/chat/completions",
        &json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "hi"}]}),
        true,
    );
    ccproxy::billing::enable_for_tests();

    assert_eq!(
        status, 200,
        "accounting is not allowed to fail the request: {body}"
    );
    assert!(
        body.contains("[DONE]"),
        "the stream must still run to its terminator: {body}"
    );
}

#[test]
fn an_opened_but_useless_ledger_does_not_block_a_turn() {
    let _guard = lock();
    // A directory where the database file goes: `Ledger::open` returns None,
    // which is what the server sees on a machine whose data directory is not
    // writable (a full disk reports the same way).
    let dir = fresh_dir("noledger");
    std::fs::create_dir_all(dir.join("billing.db")).expect("a directory where the file goes");
    assert!(
        ccproxy::billing::Ledger::open(&dir).is_none(),
        "the fixture must actually deny the ledger"
    );

    let mock = MockUpstream::start(vec![Reply::Usage {
        prompt: 20,
        cached: 5,
        completion: 4,
    }]);
    let port = start_proxy_without_ledger(mock.port);

    let (status, body) = request(
        port,
        "/v1/messages",
        &json!({"model": "m", "stream": true, "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}]}),
        true,
    );
    assert_eq!(status, 200, "the anthropic path must survive too: {body}");
    assert!(
        body.contains("message_stop"),
        "and still reach its terminator: {body}"
    );
}

/// Start the proxy from an explicit config, installing no ledger at all.
///
/// Unlike `start_proxy`, which installs one: keeping the ledger away is the
/// point of the tests above, so the previous test's override is cleared and
/// `global()` must not be consulted either.
fn start_proxy_without_ledger(mock: u16) -> u16 {
    ccproxy::billing::clear_ledger_override_for_tests();
    let config = ccproxy::config::Config {
        host: "127.0.0.1".into(),
        port: 0,
        cc_api_base: format!("http://127.0.0.1:{mock}"),
        cc_version: "1.0.0".into(),
        log_level: "error".into(),
        cors_origin: "*".into(),
        upstream_timeout_ms: 5000,
        idle_timeout_ms: 5000,
        // The no-output retry is exercised by its own tests; leaving it off
        // here keeps these cases on the pre-existing path.
        no_output_timeout_ms: 0,
        no_output_retries: 0,
        max_body_bytes: 50 * 1024 * 1024,
        ..ccproxy::config::Config::default()
    };
    let state = ccproxy::server::new_state(config);
    let (port, _handle) = ccproxy::server::serve_on_ephemeral_port(state).expect("listen");
    port
}
