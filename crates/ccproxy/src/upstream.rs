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
    /// Which transport failure this was, when `status_code` is 0.
    ///
    /// Carried as a type rather than parsed back out of `message`: the message
    /// contains the OS error text, which is localised, so classifying by text
    /// silently collapses to one bucket on a non-English Windows (see
    /// [`TransportFault`]). `None` when CC answered with a status.
    pub fault: Option<TransportFault>,
}

impl UpstreamError {
    /// The ledger tag for this failure: `http-<status>` when CC answered, and
    /// the transport class otherwise. Living here keeps the tag vocabulary next
    /// to the code that produces the statuses it names.
    pub fn error_tag(&self) -> String {
        match (self.status_code, self.fault.as_ref()) {
            (0, Some(fault)) => fault.tag().to_string(),
            (0, None) => TRANSPORT_OTHER_TAG.to_string(),
            (code, _) => format!("http-{code}"),
        }
    }

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

/// A transport-level failure, in the phase it happened.
///
/// The classes exist because "upstream unreachable" is not one problem: a
/// connect timeout, a header wait that never ends, a stalled body and a refused
/// socket have different causes and different fixes, and the whole point of the
/// tag is that a reader can tell them apart by grepping the log.
///
/// **Why not classify from the message.** ureq builds its `Display` from the
/// OS error text, which is localised — on a Chinese Windows the timeout text
/// contains no English "timeout", so the old `contains("timeout")` test filed
/// every timeout as a network error (2611 `http-network` rows, zero
/// `http-timeout`, in the production ledger). Classification therefore reads the
/// error *structure* — `ureq::ErrorKind` plus an `io::ErrorKind` down the
/// `source` chain — which is locale-independent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportFault {
    /// Could not establish TCP/TLS within the connect budget.
    ///
    /// Indistinguishable from [`Self::ConnectRefused`] when a connect timeout is
    /// set: ureq uses `connect_timeout` for the dial, and a refused socket
    /// surfaces as "... connection timed out" on Windows. An honest merge —
    /// see [`Self::ConnectRefused`].
    ConnectTimeout,
    /// Waited for the response headers past the read deadline. This is the class
    /// the 0.6.3 outage produced: a read tick shorter than a real
    /// `/alpha/generate` needs to answer.
    ///
    /// A timed-out *request write* also lands here, and deliberately: ureq
    /// reports both as `ErrorKind::Io` over an `io::ErrorKind::TimedOut` with no
    /// message, so the two are not separable from the error alone (probed — see
    /// the ADR). Inventing a second label would be a distinction the code
    /// cannot actually make. A write timeout is the rarer of the two by far,
    /// since the connect succeeded and this client writes one bounded body.
    HeaderTimeout,
    /// The socket was refused. Reported distinctly only when no connect timeout
    /// is configured (then the OS keeps the `ConnectionRefused` kind); with a
    /// connect timeout it becomes [`Self::ConnectTimeout`], because ureq routes
    /// the dial through `connect_timeout` and loses the distinction.
    ConnectRefused,
    /// DNS resolution failed.
    Dns,
    /// The connection was closed under us (reset, aborted, broken pipe, or a
    /// body that ended before its framing said it should).
    Reset,
    /// The proxy in front of the request could not be reached or did not carry
    /// it. Separate from the connect classes because the fix is different: a
    /// dead Clash is not a dead upstream.
    ProxyFailed,
    /// Anything else, with ureq's own wording preserved.
    Other(String),
}

/// Tag for a transport failure that could not be classified.
const TRANSPORT_OTHER_TAG: &str = "transport-other";

impl TransportFault {
    /// The bracket tag used in logs, e.g. `[header-timeout]`.
    ///
    /// The same strings, without the brackets, are the ledger's `errorTag`, so a
    /// log line and a ledger row name the same class identically.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::ConnectTimeout => "transport-connect-timeout",
            Self::HeaderTimeout => "transport-header-timeout",
            Self::ConnectRefused => "transport-refused",
            Self::Dns => "transport-dns",
            Self::Reset => "transport-reset",
            Self::ProxyFailed => "transport-proxy",
            Self::Other(_) => TRANSPORT_OTHER_TAG,
        }
    }

    /// A short phrase for the retry log line, e.g. `connect timeout`.
    pub fn label(&self) -> &'static str {
        match self {
            Self::ConnectTimeout => "connect timeout",
            Self::HeaderTimeout => "header timeout",
            Self::ConnectRefused => "connection refused",
            Self::Dns => "dns failure",
            Self::Reset => "connection reset",
            Self::ProxyFailed => "proxy failure",
            Self::Other(_) => "transport error",
        }
    }
}

/// Walk an error's `source` chain looking for an `io::Error`, returning its kind
/// and raw OS code.
///
/// ureq wraps the real socket error one or more levels down (a `Transport`
/// carrying an `ErrorKind::Io` wrapper carrying the io error), so the kind that
/// matters is not the one on the outermost error.
fn io_error_in_chain<'a>(err: &'a (dyn std::error::Error + 'static)) -> Option<&'a std::io::Error> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    let mut depth = 0u32;
    while let Some(e) = current {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            return Some(io);
        }
        current = e.source();
        depth = depth.saturating_add(1);
        // A self-referential or cyclic chain is not expected, but walking one
        // would hang the request thread, so the walk is bounded.
        if depth > 8 {
            return None;
        }
    }
    None
}

/// Classify a ureq error for a request that went to `url`.
///
/// Exposed so the model-catalog fetch classifies its failures the same way the
/// generation path does: one vocabulary for "the upstream did not answer",
/// whichever caller hit it. The route is resolved per-URL, because the egress
/// plan exempts loopback and `noProxy` hosts — a global "are we proxied" flag
/// would blame the proxy for faults it never carried (flagged in review).
#[must_use]
pub fn classify_for_url(err: &ureq::Error, url: &str) -> TransportFault {
    match err {
        ureq::Error::Transport(_) => classify_transport(err, crate::proxy::route_for(url)),
        // A status error has no transport fault in it; the caller (the catalog)
        // reports those by status instead of asking for a class.
        ureq::Error::Status(..) => TransportFault::Other(err.to_string()),
    }
}

/// Classify a ureq transport error into a [`TransportFault`].
///
/// Thin on purpose: it extracts what ureq offers (`ErrorKind`, the io error
/// down the source chain) and delegates to [`fault_from`], which is the whole
/// decision as a pure function and is pinned by a table test — the shapes a
/// real socket cannot be made to produce reliably (a NAT gateway answering a
/// connect with RST) are covered there instead of by a network-dependent test.
fn classify_transport(err: &ureq::Error, route: crate::proxy::Route) -> TransportFault {
    let ureq::Error::Transport(t) = err else {
        // A status error is handled by the caller; reaching here means a call
        // site misrouted one, which is still better than a panic.
        return TransportFault::Other(err.to_string());
    };
    fault_from(
        t.kind(),
        io_error_in_chain(t).map(std::io::Error::kind),
        route,
        t.to_string(),
    )
}

/// The whole transport classification as a pure, total function.
///
/// `unclassified` is the text carried verbatim into
/// [`TransportFault::Other`] when no structure matches — the caller passes the
/// error's Display so the raw detail survives.
///
/// The order of the checks is the design: the proxy/DNS kinds are
/// unambiguous, then the io kind refines the timeout families, and only
/// whatever remains falls through to `Other`. Probed on real sockets (see the
/// ADR): a header wait surfaces as `ErrorKind::Io` over `TimedOut`, a connect
/// failure as `ConnectionFailed` over `TimedOut` (with a connect timeout set)
/// or `ConnectionRefused` (without), DNS as `Dns`.
fn fault_from(
    ureq_kind: ureq::ErrorKind,
    io_kind: Option<std::io::ErrorKind>,
    route: crate::proxy::Route,
    unclassified: String,
) -> TransportFault {
    use std::io::ErrorKind;
    let via_proxy = route == crate::proxy::Route::ViaProxy;

    match ureq_kind {
        ureq::ErrorKind::Dns => {
            return if via_proxy {
                TransportFault::ProxyFailed
            } else {
                TransportFault::Dns
            };
        }
        ureq::ErrorKind::ProxyConnect | ureq::ErrorKind::ProxyUnauthorized => {
            return TransportFault::ProxyFailed;
        }
        _ => {}
    }

    match io_kind {
        Some(ErrorKind::TimedOut | ErrorKind::WouldBlock) => match ureq_kind {
            // The dial is what timed out, so it is either the proxy or the
            // upstream that would not accept a connection. Which one to check
            // first is exactly what the operator needs to know.
            ureq::ErrorKind::ConnectionFailed => {
                if via_proxy {
                    TransportFault::ProxyFailed
                } else {
                    TransportFault::ConnectTimeout
                }
            }
            // A timeout with no connect failure is a socket read: for this
            // client that is the wait for the response headers, since the body
            // is read through `UpstreamStream`, which classifies its own.
            _ => TransportFault::HeaderTimeout,
        },
        Some(ErrorKind::ConnectionRefused) => {
            if via_proxy {
                TransportFault::ProxyFailed
            } else {
                TransportFault::ConnectRefused
            }
        }
        Some(
            ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof,
        ) => TransportFault::Reset,
        _ => {
            // No io error to read, or an io kind with no better home: keep
            // ureq's own class rather than inventing one.
            match ureq_kind {
                ureq::ErrorKind::ConnectionFailed => TransportFault::ConnectTimeout,
                _ => TransportFault::Other(unclassified),
            }
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
    /// How long the stream may produce no bytes at all before this is reported
    /// as [`StreamFailure::NoOutput`]. `0` disables the check.
    ///
    /// Distinct from `idle_timeout_ms`: this measures the wait for the *first*
    /// byte — the window in which a caller may discard the attempt and re-send
    /// from scratch — while `idle_timeout_ms` measures a stall after bytes have
    /// begun to flow.
    no_output_timeout_ms: u64,
    /// The socket read timeout, which is also the granularity at which silence
    /// is accounted (see `silent_ms`).
    tick_ms: u64,
    /// Whether any byte has arrived. Once true the no-output deadline no longer
    /// applies: the attempt is no longer retryable from scratch, only resumable.
    bytes_seen: bool,
    /// Accumulated silence, reset by every byte.
    ///
    /// `ureq` arms one socket read timeout for the connection's whole life and
    /// cannot adjust it after the headers arrive, so a single socket timeout
    /// cannot express "no byte for 30 s" and "no byte for 120 s after the first
    /// one" at once. A timed-out read leaves the connection readable (measured:
    /// six consecutive 2 s timeouts, connection still alive — ledger §26), so
    /// the silence is accumulated here instead: each timed-out read blocked for
    /// `tick_ms`, so it adds exactly that, and the thresholds are applied to
    /// the total. Counting the tick rather than wall time keeps this
    /// deterministic (a reader that returns `TimedOut` at once still advances)
    /// and cannot spin.
    silent_ms: u64,
    /// Bytes read but not yet split into a complete line.
    partial: Vec<u8>,
    /// Complete lines from the last chunk that have not been returned yet.
    pending: std::collections::VecDeque<Vec<u8>>,
    done: bool,
}

impl UpstreamStream {
    pub fn new(reader: Box<dyn Read + Send>, idle_timeout_ms: u64) -> Self {
        // Without a configured no-output window the socket timeout stays what
        // it always was, and silence is accounted in those units.
        Self::with_no_output(reader, idle_timeout_ms, 0)
    }

    /// As [`Self::new`], with the retry-from-scratch deadline enabled.
    ///
    /// `tick_ms` is the socket read timeout the caller armed; it is the unit of
    /// silence accounting. Callers that do not know it pass `idle_timeout_ms`.
    pub fn with_no_output(
        reader: Box<dyn Read + Send>,
        idle_timeout_ms: u64,
        no_output_timeout_ms: u64,
    ) -> Self {
        Self::with_tick(
            reader,
            idle_timeout_ms,
            no_output_timeout_ms,
            read_tick_ms(idle_timeout_ms, no_output_timeout_ms, DEFAULT_TICK_MS),
        )
    }

    /// The full constructor, with the socket tick supplied explicitly.
    pub fn with_tick(
        reader: Box<dyn Read + Send>,
        idle_timeout_ms: u64,
        no_output_timeout_ms: u64,
        tick_ms: u64,
    ) -> Self {
        Self {
            reader,
            idle_timeout_ms,
            no_output_timeout_ms,
            tick_ms: tick_ms.max(1),
            bytes_seen: false,
            silent_ms: 0,
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
                    self.bytes_seen = true;
                    self.silent_ms = 0;
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
                Err(e) => {
                    // A read timeout is a silence marker, not an end: the
                    // connection stays readable, so the elapsed silence is
                    // accounted and measured against the two deadlines before
                    // deciding. A non-timeout error is a real failure.
                    if is_timeout(&e) {
                        self.silent_ms = self.silent_ms.saturating_add(self.tick_ms);
                        if let Some(failure) = self.silence_deadline() {
                            return Err(failure);
                        }
                        // Neither deadline applies (both disabled): fall back to
                        // reporting the timeout immediately, which is what this
                        // code did before the deadlines existed. Looping instead
                        // would spin forever on a reader that never yields.
                        if self.idle_timeout_ms == 0 && self.no_output_timeout_ms == 0 {
                            return Err(classify_read_error(&e, self.tick_ms));
                        }
                        continue;
                    }
                    return Err(classify_read_error(&e, self.idle_timeout_ms));
                }
            }
        }
    }

    /// Which deadline, if any, the accumulated silence has crossed.
    ///
    /// Silence before the first byte is [`StreamFailure::NoOutput`] — the
    /// attempt can be discarded and re-sent from scratch — but only when the
    /// no-output window is configured. With it disabled, pre-first-byte silence
    /// keeps the historical [`StreamFailure::IdleTimeout`] meaning, so a proxy
    /// that never sets the new variable behaves exactly as before.
    ///
    /// After the first byte the class is always `IdleTimeout`: a retry-from-zero
    /// would duplicate an answer that has already started, so the splice path
    /// handles it instead. That distinction is the safety property here.
    fn silence_deadline(&self) -> Option<StreamFailure> {
        if !self.bytes_seen && self.no_output_timeout_ms > 0 {
            if self.silent_ms >= self.no_output_timeout_ms {
                return Some(StreamFailure::NoOutput {
                    ms: self.no_output_timeout_ms,
                });
            }
            return None;
        }
        if self.idle_timeout_ms > 0 && self.silent_ms >= self.idle_timeout_ms {
            return Some(StreamFailure::IdleTimeout {
                ms: self.idle_timeout_ms,
            });
        }
        None
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

/// Whether an I/O error is a read timeout rather than a real failure.
///
/// A timeout leaves the connection readable, so it is treated as a silence
/// marker by [`UpstreamStream::next_event`] rather than as a terminal error.
fn is_timeout(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

/// Fallback tick when neither deadline is configured.
const DEFAULT_TICK_MS: u64 = 2_000;

/// The socket read timeout, which does double duty — and that is the whole
/// subtlety here.
///
/// `ureq` applies one read timeout for a connection's whole life, so this value
/// bounds *two* different waits:
///
/// 1. The wait for the response **headers**. No application-level check can run
///    before headers arrive, so this is a real deadline, not a tick.
/// 2. Each subsequent read during the body, where the timeout is what lets
///    `UpstreamStream` wake up and account for silence.
///
/// It is therefore the smallest configured deadline and nothing smaller. It must
/// not be shortened: a "tick" of a second or two that looks harmless for (2)
/// silently becomes a two-second cap on (1), and a real `/alpha/generate` takes
/// **2.7–4.3 s** to produce headers (measured — the request carries a ~750 KB
/// prompt that has to upload and be accepted first), so every real request would
/// fail with "Error encountered in the status line". That was a live outage.
///
/// Zero deadlines are ignored: they disable a check rather than setting a target.
fn read_tick_ms(idle_timeout_ms: u64, no_output_timeout_ms: u64, fallback: u64) -> u64 {
    let mut tick = u64::MAX;
    for deadline in [idle_timeout_ms, no_output_timeout_ms] {
        if deadline > 0 {
            tick = tick.min(deadline);
        }
    }
    if tick == u64::MAX {
        // Neither is configured; fall back to the caller's value (the upstream
        // timeout in practice), so behaviour is unchanged from before.
        return fallback.max(1);
    }
    tick.max(1)
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
#[expect(
    clippy::too_many_arguments,
    reason = "the timeout set is the caller's config, passed through verbatim; bundling it would add a type with one call site"
)]
pub fn send_to_cc(
    api_base: &str,
    api_key: &str,
    cc_version: &str,
    mut body: Value,
    timeout_ms: u64,
    idle_timeout_ms: u64,
    no_output_timeout_ms: u64,
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
            no_output_timeout_ms,
        );

        match outcome {
            Ok(Attempt::Streaming(stream)) => return Ok(stream),
            Ok(Attempt::Failed(err)) => {
                if err.retryable && attempt <= MAX_RETRIES {
                    log::warn(&retry_log_line(
                        &format!("http-{}", err.status_code),
                        attempt,
                    ));
                    // This attempt is abandoned in favour of the retry, so this
                    // layer records it; the final attempt is the caller's.
                    if let Some(sink) = attempts {
                        sink.failed(&err.error_tag());
                    }
                    last = Some(err);
                    sleep_ms(RETRY_BACKOFF_MS.saturating_mul(u64::from(attempt)));
                    continue;
                }
                return Err(err);
            }
            Err(err) => {
                if err.retryable && attempt <= MAX_RETRIES {
                    // The class is in the line, not just the raw error: an
                    // operator scanning retries wants to see
                    // `[transport-connect-timeout]` vs
                    // `[transport-header-timeout]` without reading ureq's
                    // wording. Bracketed, like every other log tag.
                    log::warn(&retry_log_line(&err.error_tag(), attempt));
                    if let Some(sink) = attempts {
                        sink.failed(&err.error_tag());
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
        fault: None,
    }))
}

/// One attempt. `Err` is a transport-level failure, `Ok(Failed)` a non-2xx.
fn attempt_send(
    url: &str,
    api_key: &str,
    cc_version: &str,
    body: &Value,
    timeout_ms: u64,
    idle_timeout_ms: u64,
    no_output_timeout_ms: u64,
) -> Result<Attempt, UpstreamError> {
    let thread_id = body.get("threadId").and_then(Value::as_str).unwrap_or("");
    let working_dir = body
        .pointer("/config/workingDir")
        .and_then(Value::as_str)
        .unwrap_or("");
    let payload = serde_json::to_string(body).unwrap_or_default();

    // `timeout_connect` bounds the TCP+TLS handshake. `timeout_read` bounds one
    // socket read, and because ureq applies it for the connection's whole life —
    // with no way to adjust it once the headers are in — it is also the deadline
    // for the wait for those headers. That dual role is why `read_tick_ms`
    // returns the smallest configured deadline rather than a fraction of it: the
    // wait for headers cannot be answered by any application-level check, so a
    // short value there is not a tighter tick, it is a hard cap on how long
    // upstream may take to answer. The alternative, an overall `.timeout()`,
    // would cap the generation itself, which is forbidden.
    //
    // During the body the same value serves as the tick that lets
    // `UpstreamStream` wake up and account for silence: both deadlines are
    // enforced there, by accumulating elapsed silence across timed-out reads.
    let tick_ms = read_tick_ms(idle_timeout_ms, no_output_timeout_ms, timeout_ms);
    // The agent carries both the egress decision (proxy or direct, made once at
    // startup — see `proxy.rs`) and the per-request timeouts above, which ureq
    // only accepts at build time. Building a bare `AgentBuilder` here is what
    // made the Rust build ignore the system proxy entirely.
    let agent = crate::proxy::agent_with_timeouts(url, timeout_ms, timeout_ms, tick_ms);

    let mut request = agent.post(url).set("Content-Type", "application/json");
    // ureq's own default is `gzip`, which would hide the framing this client
    // must see; `build_headers` advertises all three explicitly.
    for (name, value) in build_headers(api_key, cc_version, thread_id, working_dir) {
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
                fault: None,
            }));
        }
        Err(err @ ureq::Error::Transport(_)) => {
            // Classified here, while the typed error still exists: parsing the
            // class back out of the message later is what broke on a localised
            // Windows (see `TransportFault`). The tag leads the message because
            // this string is both the reject log line and what the client is
            // told — `[transport-header-timeout]` is the part a reader greps
            // for, and ureq's own text follows it for the raw detail.
            let fault = classify_transport(&err, crate::proxy::route_for(url));
            return Err(UpstreamError {
                message: format!("[{}] upstream {}: {err}", fault.tag(), fault.label()),
                status_code: 0,
                retryable: true,
                fault: Some(fault),
            });
        }
    };

    Ok(Attempt::Streaming(UpstreamStream::with_tick(
        Box::new(response.into_reader()),
        idle_timeout_ms,
        no_output_timeout_ms,
        tick_ms,
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

/// One retry log line. A function so the shape is pinned by a test: the class
/// must be bracketed like every other log tag, or a reader grepping
/// `[transport-header-timeout]` will not find the retry lines at all.
fn retry_log_line(tag: &str, attempt: u32) -> String {
    format!("CC upstream [{tag}], retrying {attempt}/{MAX_RETRIES}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::Route;

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
            fault: None,
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

    /// One NDJSON line per event, the shape the upstream sends.
    fn ndjson(events: &[&str]) -> String {
        events
            .iter()
            .map(|e| format!("data: {e}\n"))
            .collect::<String>()
    }

    /// A reader that times out forever, so the silence accounting is what ends
    /// it. Each timed-out read is one tick (the socket read timeout).
    struct AlwaysTimesOut;
    impl Read for AlwaysTimesOut {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "tick"))
        }
    }

    /// Yields `data` once, then times out forever — a stall after output.
    struct DataThenStalls {
        data: Vec<u8>,
        pos: usize,
    }
    impl Read for DataThenStalls {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos < self.data.len() {
                let take = (self.data.len() - self.pos).min(buf.len());
                buf[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
                self.pos += take;
                return Ok(take);
            }
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "tick"))
        }
    }

    #[test]
    fn silence_before_the_first_byte_is_a_no_output_failure() {
        // 3 ticks of 100 ms reach exactly the 300 ms no-output window.
        let mut stream = UpstreamStream::with_tick(Box::new(AlwaysTimesOut), 5_000, 300, 100);
        let result = stream.pump(|_| Ok(()));
        assert_eq!(result, Err(StreamFailure::NoOutput { ms: 300 }));
    }

    #[test]
    fn silence_after_the_first_byte_is_an_idle_timeout_not_a_no_output() {
        // The distinction is the whole safety property: once a byte has been
        // delivered the attempt must not be re-sent from scratch (that would
        // duplicate output), only continued.
        let data = ndjson(&[r#"{"type":"text-delta","text":"hi"}"#]);
        let reader = DataThenStalls {
            data: data.into_bytes(),
            pos: 0,
        };
        // The stream delivers its event, then stalls. Five 100 ms ticks reach
        // the 500 ms idle window; the 300 ms no-output window must not apply
        // now that a byte has arrived.
        let mut stream = UpstreamStream::with_tick(Box::new(reader), 500, 300, 100);
        let first = stream.next_event().expect("first event").is_some();
        assert!(first);
        let result = stream.pump(|_| Ok(()));
        assert_eq!(
            result,
            Err(StreamFailure::IdleTimeout { ms: 500 }),
            "a stall after output must be an idle timeout"
        );
    }

    #[test]
    fn a_disabled_no_output_window_keeps_the_old_idle_behaviour() {
        // A proxy that never sets CC_NO_OUTPUT_TIMEOUT_MS must behave exactly
        // as before: silence before the first byte is an idle timeout, and the
        // read does not spin.
        let mut stream = UpstreamStream::with_tick(Box::new(AlwaysTimesOut), 300, 0, 100);
        let result = stream.pump(|_| Ok(()));
        assert_eq!(result, Err(StreamFailure::IdleTimeout { ms: 300 }));
    }

    #[test]
    fn bytes_reset_the_accumulated_silence() {
        // Two silent ticks, a byte, then two more: if the byte did not reset
        // the counter the second pair would reach 400 ms and trip the 300 ms
        // window. The event must come through instead.
        // did not reset the counter the second pair would reach 400 ms and trip
        // a 300 ms window. It must survive instead.
        let data = ndjson(&[r#"{"type":"text-delta","text":"hi"}"#]);
        struct Pattern {
            steps: Vec<u8>,
            i: usize,
            data: Vec<u8>,
            pos: usize,
        }
        impl Read for Pattern {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let Some(step) = self.steps.get(self.i).copied() else {
                    return Ok(0);
                };
                self.i += 1;
                if step == 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "tick"));
                }
                if self.pos >= self.data.len() {
                    return Ok(0);
                }
                let take = (self.data.len() - self.pos).min(buf.len());
                buf[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
                self.pos += take;
                Ok(take)
            }
        }
        // 0,0 = 200 ms silent; 1 = a byte (resets); 0,0 = another 200 ms.
        let reader = Pattern {
            steps: vec![0, 0, 1, 0, 0],
            i: 0,
            data: data.into_bytes(),
            pos: 0,
        };
        let mut stream = UpstreamStream::with_tick(Box::new(reader), 5_000, 300, 100);
        // The event must come through; the silence never reaches 300 ms.
        let event = stream.next_event().expect("no failure").expect("an event");
        assert_eq!(event.kind, "text-delta");
    }

    #[test]
    fn the_read_tick_is_the_smallest_deadline_not_a_fraction_of_it() {
        // This value is also `ureq`'s timeout for reading the response *headers*,
        // so shortening it caps how long upstream may take to answer at all.
        // Halving or capping it at 2 s (both tried) made every real request fail:
        // `/alpha/generate` needs 2.7–4.3 s to emit headers for a ~750 KB prompt.
        assert_eq!(read_tick_ms(120_000, 30_000, 1), 30_000); // the shipped default
        assert_eq!(read_tick_ms(5_000, 30_000, 1), 5_000);
        assert_eq!(read_tick_ms(600, 30_000, 1), 600);
        assert_eq!(read_tick_ms(0, 30_000, 1), 30_000);
        assert_eq!(read_tick_ms(0, 0, 600_000), 600_000); // neither set: unchanged
        assert_eq!(read_tick_ms(0, 1, 1), 1);
    }

    #[test]
    fn the_shipped_defaults_leave_room_for_a_real_request_to_answer() {
        // Guards the outage directly: with the default 30 s no-output deadline
        // (config's `NO_OUTPUT_TIMEOUT_DEFAULT_MS`) and a 120 s idle window, the
        // header wait must be measured in seconds, not in one or two.
        let tick = read_tick_ms(120_000, 30_000, 600_000);
        assert!(
            tick >= 10_000,
            "a header wait of {tick}ms cannot accommodate a real /alpha/generate (2.7–4.3 s)"
        );
    }

    // ── transport classification ──────────────────────────
    //
    // These drive ureq against real sockets rather than a hand-built error, so
    // they pin ureq's actual structure: the class is read from `ErrorKind` plus
    // the io error down the source chain, and a hand-built error could encode
    // the wrong shape and still pass.

    use std::net::TcpListener;

    /// Port that nothing listens on: bind, read the port, drop the listener.
    fn dead_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        drop(l);
        port
    }

    /// A server that accepts the connection and then sends nothing at all.
    fn accepts_then_silent() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(mut c) = conn else { continue };
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    for _ in 0..8 {
                        if c.read(&mut buf).unwrap_or(0) == 0 {
                            break;
                        }
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    std::thread::sleep(Duration::from_secs(30));
                });
            }
        });
        port
    }

    fn quick_agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_millis(300))
            .timeout_write(Duration::from_millis(300))
            .timeout_read(Duration::from_millis(300))
            .build()
    }

    #[test]
    fn a_wait_for_headers_that_never_come_is_a_header_timeout() {
        // The class the 0.6.3 outage produced. It must NOT be reported as a
        // connect failure: the socket connected fine, the answer never came.
        let port = accepts_then_silent();
        let err = quick_agent()
            .get(&format!("http://127.0.0.1:{port}/x"))
            .call()
            .expect_err("the server never answers");
        assert_eq!(
            classify_transport(&err, Route::Direct),
            TransportFault::HeaderTimeout
        );
    }

    #[test]
    fn the_fault_table_covers_every_branch_deterministically() {
        // `fault_from` is the whole classifier as a pure function, so the full
        // mapping is pinned here — including the shapes a real socket cannot be
        // made to produce reliably (a NAT gateway answering a connect with RST
        // would be `ConnectionRefused` where the CI network usually times out;
        // rows 10 and 11 pin both readings of that world).
        use std::io::ErrorKind as Io;
        use ureq::ErrorKind as Uk;
        let unclassified = "ureq's own wording";
        let cases: &[(Uk, Option<Io>, Route, TransportFault)] = &[
            (Uk::Dns, None, Route::Direct, TransportFault::Dns),
            (Uk::Dns, None, Route::ViaProxy, TransportFault::ProxyFailed),
            (
                Uk::ProxyConnect,
                None,
                Route::Direct,
                TransportFault::ProxyFailed,
            ),
            (
                Uk::ProxyUnauthorized,
                Some(Io::ConnectionRefused),
                Route::ViaProxy,
                TransportFault::ProxyFailed,
            ),
            (
                Uk::ConnectionFailed,
                Some(Io::TimedOut),
                Route::Direct,
                TransportFault::ConnectTimeout,
            ),
            (
                Uk::ConnectionFailed,
                Some(Io::WouldBlock),
                Route::Direct,
                TransportFault::ConnectTimeout,
            ),
            (
                Uk::ConnectionFailed,
                Some(Io::TimedOut),
                Route::ViaProxy,
                TransportFault::ProxyFailed,
            ),
            (
                Uk::Io,
                Some(Io::TimedOut),
                Route::Direct,
                TransportFault::HeaderTimeout,
            ),
            (
                Uk::BadStatus,
                Some(Io::TimedOut),
                Route::ViaProxy,
                TransportFault::HeaderTimeout,
            ),
            (
                Uk::ConnectionFailed,
                Some(Io::ConnectionRefused),
                Route::Direct,
                TransportFault::ConnectRefused,
            ),
            (
                Uk::Io,
                Some(Io::ConnectionRefused),
                Route::Direct,
                TransportFault::ConnectRefused,
            ),
            (
                Uk::ConnectionFailed,
                Some(Io::ConnectionRefused),
                Route::ViaProxy,
                TransportFault::ProxyFailed,
            ),
            (
                Uk::ConnectionFailed,
                Some(Io::ConnectionReset),
                Route::Direct,
                TransportFault::Reset,
            ),
            (
                Uk::Io,
                Some(Io::BrokenPipe),
                Route::ViaProxy,
                TransportFault::Reset,
            ),
            (
                Uk::ConnectionFailed,
                None,
                Route::Direct,
                TransportFault::ConnectTimeout,
            ),
            (
                Uk::Io,
                None,
                Route::Direct,
                TransportFault::Other(unclassified.into()),
            ),
            (
                Uk::BadHeader,
                Some(Io::InvalidInput),
                Route::Direct,
                TransportFault::Other(unclassified.into()),
            ),
        ];
        for (i, (ureq_kind, io_kind, route, want)) in cases.iter().enumerate() {
            let got = fault_from(*ureq_kind, *io_kind, *route, unclassified.into());
            assert_eq!(got, *want, "case {i}: {ureq_kind:?}/{io_kind:?}/{route:?}");
        }
    }

    #[test]
    fn classification_reads_structure_not_the_localised_wording() {
        // The pin the old message-matching tag failed: a Chinese-locale Windows
        // reports the timeout with no English word anywhere in the OS text. The
        // chain is modelled as probed — a non-io layer (ureq's Transport, whose
        // value cannot be built outside the crate, stands in as a custom error)
        // whose source is the io error carrying the real kind and the localised
        // OS text. The walk must skip the non-io layer and read the kind.
        #[derive(Debug)]
        struct TransportStandIn(std::io::Error);
        impl std::fmt::Display for TransportStandIn {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(
                    f,
                    "Network Error: Error encountered in the status line: {}",
                    self.0
                )
            }
        }
        impl std::error::Error for TransportStandIn {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let os_text = "由于连接方在一段时间后没有正确答复或连接的主机没有反应，连接尝试失败。 (os error 10060)";
        let wrapped = TransportStandIn(std::io::Error::new(std::io::ErrorKind::TimedOut, os_text));
        let found = io_error_in_chain(&wrapped).expect("the io error sits one level down");
        assert_eq!(found.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            !found.to_string().to_lowercase().contains("timeout"),
            "the fixture stopped being the localised text this test exists for"
        );
    }

    #[test]
    fn the_retry_line_carries_the_class_in_brackets() {
        assert_eq!(
            retry_log_line("transport-header-timeout", 1),
            "CC upstream [transport-header-timeout], retrying 1/2..."
        );
    }

    #[test]
    fn a_refused_socket_is_distinguished_when_no_connect_timeout_is_set() {
        // With no connect timeout the OS keeps the refused kind, so the split
        // is real. (With one, ureq's dial reports a timeout instead — that
        // merge is documented on `TransportFault`.)
        let err = ureq::AgentBuilder::new()
            .timeout_read(Duration::from_millis(2000))
            .build()
            .get(&format!("http://127.0.0.1:{}/x", dead_port()))
            .call()
            .expect_err("nothing listens there");
        assert_eq!(
            classify_transport(&err, Route::Direct),
            TransportFault::ConnectRefused
        );
    }

    #[test]
    fn a_resolution_failure_is_dns_direct_and_proxy_failure_proxied() {
        let err = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_millis(2000))
            .build()
            .get("https://no-such-host.invalid/x")
            .call()
            .expect_err("the TLD is reserved");
        assert_eq!(classify_transport(&err, Route::Direct), TransportFault::Dns);
        // Proxied, the client resolves only the proxy's host, so a resolution
        // failure is the proxy's fault — the thing to check is Clash, not the
        // upstream.
        assert_eq!(
            classify_transport(&err, Route::ViaProxy),
            TransportFault::ProxyFailed
        );
    }

    #[test]
    fn a_dead_proxy_is_not_reported_as_a_dead_upstream() {
        let proxy_url = format!("http://127.0.0.1:{}", dead_port());
        let err = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_millis(400))
            .proxy(ureq::Proxy::new(proxy_url).expect("proxy"))
            .build()
            .get("https://api.commandcode.ai/health")
            .call()
            .expect_err("the proxy is not listening");
        assert_eq!(
            classify_transport(&err, Route::ViaProxy),
            TransportFault::ProxyFailed
        );
    }

    #[test]
    fn a_transport_failure_tags_the_ledger_and_never_collapses_to_one_bucket() {
        let mut seen = std::collections::BTreeSet::new();
        for fault in [
            TransportFault::ConnectTimeout,
            TransportFault::HeaderTimeout,
            TransportFault::ConnectRefused,
            TransportFault::Dns,
            TransportFault::Reset,
            TransportFault::ProxyFailed,
            TransportFault::Other("x".into()),
        ] {
            let err = UpstreamError {
                message: String::new(),
                status_code: 0,
                retryable: true,
                fault: Some(fault.clone()),
            };
            assert_eq!(err.error_tag(), fault.tag());
            assert!(!fault.tag().is_empty());
            seen.insert(fault.tag());
        }
        // Every class has its own tag: a shared tag would be the collapse this
        // change is meant to remove.
        assert_eq!(seen.len(), 7, "classes must not share a tag: {seen:?}");
    }

    #[test]
    fn an_answered_status_still_tags_by_status_not_by_transport() {
        let err = UpstreamError {
            message: String::new(),
            status_code: 429,
            retryable: true,
            fault: None,
        };
        assert_eq!(err.error_tag(), "http-429");
    }
}
