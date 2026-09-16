//! The health probe that decides whether a handover succeeded.
//!
//! A TCP connect is **not** sufficient evidence that "our proxy is serving":
//! the port could be held by an unrelated program, or by a process that has
//! bound but not finished starting, or by a wedged process whose socket is
//! still open. Committing on that evidence would let the incumbent exit while
//! the successor cannot actually serve — a service vacuum. So this sends a
//! real HTTP request and checks the body is `/health`-shaped.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Whether an HTTP `/health` answers on `port`, and optionally reports
/// `expected_version`.
///
/// `expected_version` is for telling "our proxy" apart from "some other program
/// that happens to hold this port". It must **not** be used mid-handover: the
/// port may still be answered by the outgoing incumbent, whose version is
/// legitimately different, and demanding a match there would turn a normal
/// handover window into a rollback.
#[must_use]
pub fn port_serving(port: u16, expected_version: Option<&str>) -> bool {
    match fetch_health(port) {
        Some(body) => health_body_is_ok(&body, expected_version),
        None => false,
    }
}

/// Send `GET /health` over a plain socket. `None` on any failure.
///
/// Hand-rolled rather than pulled from `ureq`: this is one request to loopback
/// with a 2s budget, and it must not inherit the system proxy — a set
/// `HTTPS_PROXY` would otherwise route a loopback probe through the network.
fn fetch_health(port: u16) -> Option<String> {
    let addr = format!("127.0.0.1:{port}");
    let mut stream =
        TcpStream::connect_timeout(&addr.parse().ok()?, Duration::from_millis(2000)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(2000)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(2000)))
        .ok()?;
    let request = format!(
        "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;
    parse_health_response(BufReader::new(stream))
}

/// Pull the body out of an HTTP/1.1 response, or `None` if it is not a 200.
///
/// Split out from the socket work so it can be tested against real captured
/// bytes — the header/body boundary is exactly the thing that is easy to get
/// wrong, and getting it wrong rejects healthy proxies.
fn parse_health_response(mut reader: impl BufRead) -> Option<String> {
    // Status line first.
    let mut status_line = String::new();
    reader.read_line(&mut status_line).ok()?;
    if !status_line.contains(" 200") {
        return None;
    }

    // Then the headers, up to the blank line that ends them. Skipping this is
    // not optional: the body starts after it, and handing the headers to a JSON
    // parser rejects every otherwise-healthy proxy.
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            // EOF before the headers ended: malformed response.
            return None;
        }
        if header.trim_end().is_empty() {
            break;
        }
    }

    // `Connection: close` means the server ends the stream, so reading to EOF is
    // bounded; the 4 KiB cap keeps a misbehaving server from filling memory.
    let mut body = String::new();
    let _ = reader.take(4096).read_to_string(&mut body);
    Some(body)
}

/// The body must be `/health`-shaped, not merely "some HTTP response".
///
/// The check is deliberately loose about formatting — a shape check on parsed
/// JSON — because the point is to recognise the endpoint, not to assert the
/// response byte-for-byte.
#[must_use]
pub fn health_body_is_ok(body: &str, expected_version: Option<&str>) -> bool {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    if json.get("status").and_then(|v| v.as_str()) != Some("ok") {
        return false;
    }
    match expected_version {
        Some(expected) => json.get("version").and_then(|v| v.as_str()) == Some(expected),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response captured verbatim from the proxy, headers and all.
    const REAL_RESPONSE: &str = "HTTP/1.1 200 OK\r\n\
         Server: tiny-http (Rust)\r\n\
         Date: Tue, 15 Sep 2026 18:31:43 GMT\r\n\
         Content-Type: application/json\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Content-Length: 118\r\n\
         \r\n\
         {\"status\":\"ok\",\"version\":\"0.5.0\",\"cache\":{\"requests\":0}}";

    #[test]
    fn the_body_is_extracted_after_the_headers() {
        // Regression: reading the whole stream as the body left the headers in
        // front of the JSON, so parsing failed and a healthy successor was told
        // it had failed to serve.
        let body = parse_health_response(REAL_RESPONSE.as_bytes()).expect("a 200 response");
        assert!(
            body.starts_with('{'),
            "the body must not include the headers, got {body:?}"
        );
        assert!(health_body_is_ok(&body, None));
    }

    #[test]
    fn a_non_200_is_not_a_serving_proxy() {
        let response = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
        assert!(parse_health_response(response.as_bytes()).is_none());
    }

    #[test]
    fn a_health_body_is_recognised() {
        assert!(health_body_is_ok(
            r#"{"status":"ok","version":"0.5.0"}"#,
            None
        ));
    }

    #[test]
    fn a_foreign_response_is_rejected() {
        // The exact failure the real request guards against: something is
        // listening, but it is not this endpoint.
        assert!(!health_body_is_ok("<html>hello</html>", None));
        assert!(!health_body_is_ok(r#"{"status":"starting"}"#, None));
    }

    #[test]
    fn the_version_check_only_applies_when_asked() {
        let body = r#"{"status":"ok","version":"0.5.0"}"#;
        assert!(health_body_is_ok(body, Some("0.5.0")));
        // Mid-handover the outgoing instance may legitimately answer, so an
        // unchecked probe must not fail on version.
        assert!(health_body_is_ok(body, None));
        assert!(!health_body_is_ok(body, Some("0.4.4")));
    }
}
