//! Upstream HTTP client for CC's `/alpha/generate`, ported from src/upstream.ts.
//!
//! Three things here are load-bearing and easy to get subtly wrong:
//!
//! * **No total-duration cap on a generation.** The Node version has two
//!   deadlines: one for the response headers (and a non-2xx error body), and a
//!   separate byte-interval deadline on the stream. A generation may legitimately
//!   run for minutes, so anything that caps its total duration would kill long
//!   turns. `ureq`'s `.timeout()` is exactly such a cap and is therefore not
//!   used; `.timeout_read()` bounds a single read instead, which is what the idle
//!   timeout means.
//! * **`stream: true` is forced.** CC's endpoint is always streaming; a
//!   non-streaming downstream request still reads the same NDJSON stream and
//!   collapses it. The flag the client sent is not what goes upstream.
//! * **A stalled upstream must never look like a clean EOF.** A timeout or a
//!   mid-body disconnect must surface as an error, never as an empty read —
//!   either one reported as EOF turns a truncated answer into a success.
//!
//! Retries are bounded and happen only before the response body is consumed. A
//! caller abort is never retried: the client is gone, so a retry would only
//! burn the user's upstream quota.

use crate::log;
use crate::ndjson::{parse_cc_line, CCEvent, ParsedChunk};
use crate::sse::StreamFailure;
use serde_json::Value;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

const MAX_RETRIES: u32 = 2;
const RETRY_BACKOFF_MS: u64 = 500;
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;

/// Where the retry layers report the attempts they make and abandon.
///
/// Defined here because this is the layer that *decides* to retry, and whoever
/// abandons an attempt is who accounts for it — that way the same attempt is
/// never recorded twice. See [`crate::billing::RequestLedger`].
pub trait AttemptSink: Send + Sync {
    /// Called immediately before a request really goes out. An attempt that
    /// fails mid-flight has already reached CC and may already have been billed.
    fn started(&self);
    /// Called when this layer abandons the attempt it just made in order to
    /// retry. `tag` classifies the failure, for attribution.
    fn failed(&self, tag: &str);
}

/// A failed upstream request, with the fields the error mapping needs.
#[derive(Debug, Clone)]
pub struct UpstreamError {
    pub message: String,
    /// 0 when there was no HTTP response at all (transport failure or timeout).
    pub status_code: u16,
    pub retryable: bool,
}

impl UpstreamError {
    /// The status the downstream client is told. 4xx passes through so the
    /// client sees CC's own verdict; everything else collapses to 502 — a 5xx
    /// forwarded verbatim would have the client retrying against us.
    pub fn downstream_status(&self) -> u16 {
        match self.status_code {
            400..=499 => self.status_code,
            _ => 502,
        }
    }
}

/// Build the header set the official Command Code CLI sends.
///
/// CC's server inspects these and answers "Proxy use detected" when any of the
/// CLI-identifying headers are missing or stale, so they are reproduced
/// literally rather than being treated as ordinary HTTP headers.
pub fn build_headers(
    api_key: &str,
    cc_version: &str,
    thread_id: &str,
    working_dir: &str,
) -> Vec<(String, String)> {
    vec![
        ("Accept".into(), "application/json, */*".into()),
        ("Accept-Encoding".into(), "gzip, deflate, br".into()),
        ("Accept-Language".into(), "en-US,en;q=0.9".into()),
        ("Connection".into(), "keep-alive".into()),
        (
            "User-Agent".into(),
            format!("commandcode-cli/{cc_version} Node.js/v24.16.0"),
        ),
        ("Authorization".into(), format!("Bearer {api_key}")),
        ("x-cli-environment".into(), "production".into()),
        ("x-command-code-version".into(), cc_version.into()),
        ("x-session-id".into(), thread_id.into()),
        ("x-co-flag".into(), "false".into()),
        ("x-taste-learning".into(), "false".into()),
        ("x-project-slug".into(), slugify_working_dir(working_dir)),
        ("traceparent".into(), generate_traceparent()),
    ]
}

/// The last path segment of the working directory, lowercased and reduced to
/// `[a-z0-9-]`, capped at 40 characters.
pub fn slugify_working_dir(working_dir: &str) -> String {
    let raw = if working_dir.is_empty() {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        working_dir.to_string()
    };
    let base = raw
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("commandcode-proxy");
    let slug: String = base
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    if slug.is_empty() {
        "commandcode-proxy".into()
    } else {
        slug
    }
}

/// W3C traceparent: `version-trace_id-span_id-flags` with lowercase hex ids and
/// the sampled flag set.
fn generate_traceparent() -> String {
    let trace = uuid::Uuid::new_v4().into_bytes();
    let trace_id: String = trace.iter().map(|b| format!("{b:02x}")).collect();
    let span = uuid::Uuid::new_v4().into_bytes();
    let span_id: String = span.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("00-{trace_id}-{span_id}-01")
}

/// Replace the API key and strip control characters from an upstream error body
/// before it reaches the client.
///
/// The order matters: redacting first means a key containing a character the
/// control filter would remove still matches. Tab, newline and CR survive so a
/// multi-line upstream message stays readable.
pub fn sanitize_error_text(text: &str, api_key: &str) -> String {
    let redacted = if api_key.is_empty() {
        text.to_string()
    } else {
        text.replace(api_key, "[redacted]")
    };
    redacted
        .chars()
        .filter(|c| {
            let code = *c as u32;
            !matches!(code, 0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f)
        })
        .collect()
}

/// A pump callback asked to stop, because the downstream client disconnected or
/// a write failed. Not a failure of the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpStop;

/// The upstream response body, read as NDJSON.
///
/// Pull-based and resumable: `next_event` returns one parsed event at a time
/// without buffering the rest, so a slow downstream consumer applies
/// backpressure to the upstream socket instead of growing an unbounded queue.
/// Lines left over from a partially consumed chunk are kept, so resuming never
/// loses or re-splits an event.
pub struct UpstreamStream {
    reader: Box<dyn Read + Send>,
    idle_timeout_ms: u64,
    /// Bytes read but not yet split into a complete line.
    partial: Vec<u8>,
    /// Complete lines from the last chunk that have not been returned yet.
    pending: std::collections::VecDeque<Vec<u8>>,
    done: bool,
}

impl UpstreamStream {
    pub fn new(reader: Box<dyn Read + Send>, idle_timeout_ms: u64) -> Self {
        Self {
            reader,
            idle_timeout_ms,
            partial: Vec::new(),
            pending: std::collections::VecDeque::new(),
            done: false,
        }
    }

    /// The next upstream event, or `None` at a clean end of stream.
    ///
    /// Lines that are not events (keepalives, blank lines, unparseable garbage)
    /// are skipped, so a `None` only ever means a real end.
    pub fn next_event(&mut self) -> Result<Option<CCEvent>, StreamFailure> {
        loop {
            while let Some(line) = self.pending.pop_front() {
                match parse_cc_line(&String::from_utf8_lossy(&line)) {
                    ParsedChunk::Event(e) => return Ok(Some(e)),
                    ParsedChunk::Done => {
                        self.done = true;
                        return Ok(None);
                    }
                    _ => continue,
                }
            }
            if self.done {
                return Ok(None);
            }

            // Refill pending from the socket.
            let mut buffer = [0u8; 8192];
            match self.reader.read(&mut buffer) {
                Ok(0) => {
                    self.done = true;
                    // Trailing bytes with no newline terminator are still a line.
                    if !self.partial.is_empty() {
                        let line = std::mem::take(&mut self.partial);
                        if let ParsedChunk::Event(e) =
                            parse_cc_line(&String::from_utf8_lossy(&line))
                        {
                            return Ok(Some(e));
                        }
                    }
                    return Ok(None);
                }
                Ok(n) => {
                    // Bytes are accumulated before splitting: a multi-byte
                    // character straddling two reads would decode as garbage if
                    // each chunk were decoded on its own.
                    self.partial
                        .extend_from_slice(buffer.get(..n).unwrap_or(&[]));
                    while let Some(idx) = self.partial.iter().position(|b| *b == b'\n') {
                        let mut line: Vec<u8> = self.partial.drain(..=idx).collect();
                        // Drop the terminator; `parse_cc_line` trims the rest.
                        while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                            line.pop();
                        }
                        self.pending.push_back(line);
                    }
                }
                Err(e) => return Err(classify_read_error(&e, self.idle_timeout_ms)),
            }
        }
    }

    /// Drain the stream, handing each event to `on_event`. Returns `Err` for an
    /// upstream failure; a callback returning `Err` is reported as the client
    /// going away, which is not an upstream fault.
    pub fn pump(
        &mut self,
        mut on_event: impl FnMut(CCEvent) -> Result<(), PumpStop>,
    ) -> Result<(), StreamFailure> {
        while let Some(event) = self.next_event()? {
            if on_event(event).is_err() {
                return Err(StreamFailure::ClientGone);
            }
        }
        Ok(())
    }
}

/// Classify an I/O error from the upstream body.
///
/// ureq's chunked decoder reports a truncated body as `InvalidInput` with the
/// message "Error while decoding chunks" rather than as a socket error: it hits
/// EOF while reading a chunk header, which is what a dropped connection looks
/// like from inside the framing layer. That is a mid-body failure, not a clean
/// end — leaving it unclassified would report a truncated answer as a
/// successful turn.
fn classify_read_error(err: &std::io::Error, idle_timeout_ms: u64) -> StreamFailure {
    use std::io::ErrorKind;
    match err.kind() {
        // ureq normalises a socket read timeout to TimedOut.
        ErrorKind::TimedOut | ErrorKind::WouldBlock => StreamFailure::IdleTimeout {
            ms: idle_timeout_ms,
        },
        ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::BrokenPipe
        | ErrorKind::UnexpectedEof => StreamFailure::ConnectionReset,
        _ => {
            let msg = err.to_string();
            // A disconnect mid-body surfaces as a transport error whose wording
            // varies; the downstream message contract expects this class.
            if msg.contains("terminated")
                || msg.contains("socket hang up")
                || msg.contains("Error while decoding chunks")
            {
                StreamFailure::ConnectionReset
            } else {
                StreamFailure::Other(msg)
            }
        }
    }
}

/// The outcome of a request attempt that reached the point of having a response.
pub enum Attempt {
    Streaming(UpstreamStream),
    Failed(UpstreamError),
}

/// Send one request to `/alpha/generate` with the retry matrix applied.
///
/// The `threadId` is part of `body` and is therefore reused across attempts: CC
/// bills per session, so a retry must not look like a second one.
///
/// Every attempt re-sends the same body. Nothing here re-resolves the model —
/// a name CC rejects needs a catalog refresh, which is a different layer
/// (`generate::send_with_model_discovery`), and doing it inside this loop would
/// retry a rejection that a retry cannot fix.
pub fn send_to_cc(
    api_base: &str,
    api_key: &str,
    cc_version: &str,
    mut body: Value,
    timeout_ms: u64,
    idle_timeout_ms: u64,
    attempts: Option<&Arc<dyn AttemptSink>>,
) -> Result<UpstreamStream, UpstreamError> {
    // CC's endpoint is always streaming; the downstream `stream` flag is
    // applied when the collected events are collapsed into a response.
    if let Some(params) = body.get_mut("params").and_then(Value::as_object_mut) {
        params.insert("stream".into(), Value::Bool(true));
    }

    let url = format!("{}/alpha/generate", api_base.trim_end_matches('/'));
    let mut last: Option<UpstreamError> = None;

    for attempt in 1..=MAX_RETRIES + 1 {
        // Reported before the request goes out: a request that fails mid-flight
        // has already reached CC, so it is an attempt that may have been billed.
        if let Some(sink) = attempts {
            sink.started();
        }
        let outcome = attempt_send(
            &url,
            api_key,
            cc_version,
            &body,
            timeout_ms,
            idle_timeout_ms,
        );

        match outcome {
            Ok(Attempt::Streaming(stream)) => return Ok(stream),
            Ok(Attempt::Failed(err)) => {
                if err.retryable && attempt <= MAX_RETRIES {
                    log::warn(&format!(
                        "CC upstream {}, retrying {attempt}/{MAX_RETRIES}...",
                        err.status_code
                    ));
                    // This attempt is abandoned in favour of the retry, so this
                    // layer records it; the final attempt is the caller's.
                    if let Some(sink) = attempts {
                        sink.failed(&format!("http-{}", err.status_code));
                    }
                    last = Some(err);
                    sleep_ms(RETRY_BACKOFF_MS.saturating_mul(u64::from(attempt)));
                    continue;
                }
                return Err(err);
            }
            Err(err) => {
                if err.retryable && attempt <= MAX_RETRIES {
                    log::warn(&format!(
                        "CC upstream timeout/error, retrying {attempt}/{MAX_RETRIES}..."
                    ));
                    if let Some(sink) = attempts {
                        // status_code 0 means no HTTP response at all, so the
                        // distinction is transport failure vs. timeout.
                        sink.failed(timeout_tag(&err));
                    }
                    last = Some(err);
                    sleep_ms(RETRY_BACKOFF_MS.saturating_mul(u64::from(attempt)));
                    continue;
                }
                return Err(err);
            }
        }
    }

    Err(last.unwrap_or(UpstreamError {
        message: "Upstream request failed after retries".into(),
        status_code: 0,
        retryable: true,
    }))
}

/// Classify a transport failure for the ledger.
///
/// A read that exceeded the idle deadline and a connection that broke look the
/// same from the outside but are different problems, so the tag keeps them
/// apart: one is an upstream stall, the other a dropped socket.
fn timeout_tag(err: &UpstreamError) -> &'static str {
    let lowered = err.message.to_lowercase();
    if lowered.contains("timed out") || lowered.contains("timeout") {
        "http-timeout"
    } else {
        "http-network"
    }
}

/// One attempt. `Err` is a transport-level failure, `Ok(Failed)` a non-2xx.
fn attempt_send(
    url: &str,
    api_key: &str,
    cc_version: &str,
    body: &Value,
    timeout_ms: u64,
    idle_timeout_ms: u64,
) -> Result<Attempt, UpstreamError> {
    let thread_id = body.get("threadId").and_then(Value::as_str).unwrap_or("");
    let working_dir = body
        .pointer("/config/workingDir")
        .and_then(Value::as_str)
        .unwrap_or("");
    let payload = serde_json::to_string(body).unwrap_or_default();

    // `timeout_connect` bounds the TCP+TLS handshake. `timeout_read` bounds a
    // single read, which is what detects a stalled stream — note this also
    // bounds the wait for the response headers, so the header deadline is the
    // idle deadline rather than the (usually larger) upstream timeout. ureq
    // applies one socket read timeout for the connection's whole life and does
    // not expose it for adjustment after the headers arrive, and the alternative
    // (an overall `.timeout()`) would cap the generation, which is forbidden.
    let mut agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(timeout_ms))
        .timeout_write(Duration::from_millis(timeout_ms));
    if idle_timeout_ms > 0 {
        agent = agent.timeout_read(Duration::from_millis(idle_timeout_ms));
    } else {
        // CC_IDLE_TIMEOUT_MS=0 disables idle detection entirely.
        agent = agent.timeout_read(Duration::from_millis(timeout_ms));
    }
    let agent = agent.build();

    let mut request = agent
        .post(url)
        .set("Content-Type", "application/json")
        // ureq's own default is `gzip`, which would hide the framing this
        // client must see; the CLI advertises all three explicitly.
        .set("Accept-Encoding", "gzip, deflate, br");
    for (name, value) in build_headers(api_key, cc_version, thread_id, working_dir) {
        if name.eq_ignore_ascii_case("accept-encoding") {
            continue;
        }
        request = request.set(&name, &value);
    }

    let response = match request.send_string(&payload) {
        Ok(r) => r,
        Err(ureq::Error::Status(code, response)) => {
            // A non-2xx still carries a body worth reporting. The read of that
            // body is bounded by the socket timeout set above.
            let text = read_error_body(response.into_reader());
            let retryable = code >= 500 || code == 429;
            return Ok(Attempt::Failed(UpstreamError {
                message: format!("CC API {code}: {}", sanitize_error_text(&text, api_key)),
                status_code: code,
                retryable,
            }));
        }
        Err(ureq::Error::Transport(t)) => {
            return Err(UpstreamError {
                message: format!("Upstream request failed: {t}"),
                status_code: 0,
                retryable: true,
            });
        }
    };

    Ok(Attempt::Streaming(UpstreamStream::new(
        Box::new(response.into_reader()),
        idle_timeout_ms,
    )))
}

/// Read an error body with the 16 KiB cap.
///
/// Over the cap the whole body is replaced rather than returning a prefix: a
/// truncation point could land halfway through the API key, and the key is
/// redacted by exact match.
fn read_error_body(reader: impl Read) -> String {
    let mut chunks: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut reader = reader;
    let mut buffer = [0u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                if total.saturating_add(n) >= MAX_ERROR_BODY_BYTES {
                    return "[error body truncated]".into();
                }
                chunks.extend_from_slice(buffer.get(..n).unwrap_or(&[]));
                total = total.saturating_add(n);
            }
            Err(_) => return "[error body unavailable]".into(),
        }
    }
    String::from_utf8_lossy(&chunks).into_owned()
}

/// Sleep between retries in slices, so a shutdown is not held by a backoff.
fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_dir_becomes_a_slug() {
        assert_eq!(slugify_working_dir("/home/me/My Project"), "my-project");
        assert_eq!(slugify_working_dir("D:\\Projects\\cc-proxy"), "cc-proxy");
        assert_eq!(slugify_working_dir(&"a".repeat(80)).len(), 40);
        // An empty working dir falls back to the process cwd (which is this
        // crate's directory under `cargo test`), so only the placeholder cases
        // can be asserted exactly.
        assert_eq!(slugify_working_dir("///"), "commandcode-proxy");
        assert_eq!(slugify_working_dir("/"), "commandcode-proxy");
        assert!(!slugify_working_dir("").is_empty());
    }

    #[test]
    fn the_api_key_is_redacted_before_control_characters_are_stripped() {
        let out = sanitize_error_text(
            "bad key sk-secret-123\nretry later\u{0007}",
            "sk-secret-123",
        );
        assert_eq!(out, "bad key [redacted]\nretry later");
    }

    #[test]
    fn tabs_newlines_and_carriage_returns_survive() {
        let out = sanitize_error_text("a\tb\nc\rd\u{0000}\u{007f}", "");
        assert_eq!(out, "a\tb\nc\rd");
    }

    #[test]
    fn status_mapping_passes_4xx_through_and_collapses_the_rest() {
        let mk = |code| UpstreamError {
            message: String::new(),
            status_code: code,
            retryable: false,
        };
        assert_eq!(mk(401).downstream_status(), 401);
        assert_eq!(mk(403).downstream_status(), 403);
        assert_eq!(mk(429).downstream_status(), 429);
        assert_eq!(mk(400).downstream_status(), 400);
        assert_eq!(mk(500).downstream_status(), 502);
        assert_eq!(mk(529).downstream_status(), 502);
        // No HTTP response at all (transport failure or timeout).
        assert_eq!(mk(0).downstream_status(), 502);
    }

    #[test]
    fn headers_carry_every_cli_identifier() {
        let h = build_headers("k", "1.2.3", "thread-1", "/x/my-project");
        let get = |name: &str| {
            h.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        for name in [
            "Accept",
            "Accept-Encoding",
            "Accept-Language",
            "Connection",
            "User-Agent",
            "Authorization",
            "x-cli-environment",
            "x-command-code-version",
            "x-session-id",
            "x-co-flag",
            "x-taste-learning",
            "x-project-slug",
            "traceparent",
        ] {
            assert!(!get(name).is_empty(), "{name} must be present");
        }
        assert_eq!(get("Authorization"), "Bearer k");
        assert_eq!(get("x-session-id"), "thread-1");
        assert_eq!(get("x-project-slug"), "my-project");
        assert_eq!(get("x-cli-environment"), "production");
        assert_eq!(get("Accept"), "application/json, */*");
        assert_eq!(get("Accept-Encoding"), "gzip, deflate, br");
        assert_eq!(get("Connection"), "keep-alive");
        assert!(get("User-Agent").starts_with("commandcode-cli/1.2.3 Node.js/"));

        let tp = get("traceparent");
        let parts: Vec<&str> = tp.split('-').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "00");
        assert_eq!(parts[1].len(), 32);
        assert_eq!(parts[2].len(), 16);
        assert_eq!(parts[3], "01");
        assert!(parts[1]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn an_error_body_over_the_cap_is_truncated_not_prefixed() {
        let big = vec![b'x'; MAX_ERROR_BODY_BYTES + 10];
        assert_eq!(
            read_error_body(std::io::Cursor::new(big)),
            "[error body truncated]"
        );
        assert_eq!(
            read_error_body(std::io::Cursor::new(b"{\"error\":\"nope\"}".to_vec())),
            "{\"error\":\"nope\"}"
        );
    }

    #[test]
    fn read_errors_are_classified_by_kind_not_wording() {
        let timed = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        assert_eq!(
            classify_read_error(&timed, 800),
            StreamFailure::IdleTimeout { ms: 800 }
        );
        // On Windows a socket timeout can surface as WouldBlock.
        let blocked = std::io::Error::new(std::io::ErrorKind::WouldBlock, "again");
        assert_eq!(
            classify_read_error(&blocked, 800),
            StreamFailure::IdleTimeout { ms: 800 }
        );
        let reset = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        assert_eq!(
            classify_read_error(&reset, 800),
            StreamFailure::ConnectionReset
        );
        let eof = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof");
        assert_eq!(
            classify_read_error(&eof, 800),
            StreamFailure::ConnectionReset
        );
    }

    #[test]
    fn a_stalled_stream_reports_the_idle_timeout_not_eof() {
        // A reader that always fails with a timeout must produce the idle
        // timeout failure, because reporting it as EOF would turn a truncated
        // answer into a successful one.
        struct Stalled;
        impl Read for Stalled {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "stalled"))
            }
        }
        let mut stream = UpstreamStream::new(Box::new(Stalled), 800);
        let result = stream.pump(|_| Ok(()));
        assert_eq!(result, Err(StreamFailure::IdleTimeout { ms: 800 }));
    }

    #[test]
    fn lines_split_across_reads_reassemble() {
        // A multi-byte character and a JSON line both straddle the read
        // boundary; the pump must decode the assembled bytes, not the chunks.
        struct Chunky {
            data: Vec<u8>,
            pos: usize,
        }
        impl Read for Chunky {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.pos >= self.data.len() {
                    return Ok(0);
                }
                // Three bytes at a time: enough to split the 3-byte character.
                let take = (self.data.len() - self.pos).min(3);
                buf[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
                self.pos += take;
                Ok(take)
            }
        }

        let payload = "data: {\"type\":\"text-delta\",\"data\":{\"text\":\"日\"}}\n\
                       data: {\"type\":\"finish\",\"data\":{}}\n";
        let mut stream = UpstreamStream::new(
            Box::new(Chunky {
                data: payload.as_bytes().to_vec(),
                pos: 0,
            }),
            800,
        );
        let mut kinds: Vec<String> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        stream
            .pump(|e| {
                if let Some(t) = e.data.get("text").and_then(Value::as_str) {
                    texts.push(t.to_string());
                }
                kinds.push(e.kind);
                Ok(())
            })
            .expect("pump succeeds");
        assert_eq!(kinds, vec!["text-delta", "finish"]);
        assert_eq!(texts, vec!["日"]);
    }

    #[test]
    fn a_callback_stop_is_reported_as_the_client_going_away() {
        struct Once;
        impl Read for Once {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let data = b"data: {\"type\":\"start\",\"data\":{}}\n";
                if buf.len() < data.len() {
                    return Ok(0);
                }
                buf[..data.len()].copy_from_slice(data);
                Ok(data.len())
            }
        }
        let mut stream = UpstreamStream::new(Box::new(Once), 800);
        let result = stream.pump(|_| Err(PumpStop));
        assert_eq!(result, Err(StreamFailure::ClientGone));
    }
}
