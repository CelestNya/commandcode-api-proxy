//! The streaming body's behaviour under the conditions the encoder tests
//! cannot reach: a failing transport, a replacement attempt, and a reader that
//! does not drain the body in one call.
//!
//! `SseBody` is what decides whether a failure becomes an in-band error or a
//! silent truncation, and whether a re-send is invisible or duplicates output.
//! Both are driven here with fake upstreams rather than sockets, because the
//! decision is made from encoder state and delivered-byte counts — not from
//! anything the network reports.

use ccproxy::sse::StreamFailure;
use ccproxy::stream_body::{Dialect, SseBody};
use ccproxy::translate::{AnthropicEncoder, OpenAIEncoder};
use ccproxy::upstream::UpstreamStream;
use std::io::Read;
use std::sync::Arc;

/// A reader that yields the given bytes and then reports a transport failure.
struct Failing {
    data: Vec<u8>,
    pos: usize,
    failure: Option<StreamFailure>,
}

impl Failing {
    fn clean(data: &str) -> Self {
        Self {
            data: data.as_bytes().to_vec(),
            pos: 0,
            failure: None,
        }
    }

    fn then_fail(data: &str, failure: StreamFailure) -> Self {
        Self {
            data: data.as_bytes().to_vec(),
            pos: 0,
            failure: Some(failure),
        }
    }
}

impl Read for Failing {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.data.len() {
            let take = (self.data.len() - self.pos).min(buf.len());
            buf[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
            self.pos += take;
            return Ok(take);
        }
        match &self.failure {
            // An idle timeout is what ureq's socket timeout surfaces as.
            Some(StreamFailure::IdleTimeout { .. }) => {
                Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "stalled"))
            }
            Some(f) => Err(std::io::Error::other(f.tagged())),
            None => Ok(0),
        }
    }
}

fn ndjson(events: &[&str]) -> String {
    events
        .iter()
        .map(|e| format!("data: {e}\n"))
        .collect::<String>()
}

fn body_of(reader: Failing, dialect: Dialect, reconnect: Option<&str>) -> SseBody {
    let stream = UpstreamStream::new(Box::new(reader), 800);
    let reconnect = reconnect.map(|data| {
        let bytes = ndjson(&[data]);
        let boxed: Box<dyn FnMut() -> Result<UpstreamStream, StreamFailure> + Send> =
            Box::new(move || Ok(UpstreamStream::new(Box::new(Failing::clean(&bytes)), 800)));
        boxed
    });
    SseBody::new(stream, dialect, "m", reconnect, SseBody::new_slot())
}

fn read_all(body: &mut SseBody) -> String {
    let mut out = Vec::new();
    body.read_to_end(&mut out).expect("body reads");
    String::from_utf8_lossy(&out).into_owned()
}

/// Read in fixed-size bites, which is what a slow consumer looks like to the
/// body: every `read` returns before the produced bytes are exhausted, so the
/// leftover buffer has to survive across calls.
fn read_in_bites(body: &mut SseBody, bite: usize) -> String {
    let mut out: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; bite];
    loop {
        let n = body.read(&mut buf).expect("body reads");
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&out).into_owned()
}

const OPENAI_EVENTS: &[&str] = &[
    r#"{"type":"start","data":{}}"#,
    r#"{"type":"text-delta","data":{"text":"hi"}}"#,
    r#"{"type":"finish","data":{"finishReason":"stop"}}"#,
];

#[test]
fn a_clean_stream_ends_with_the_done_sentinel() {
    let mut body = body_of(
        Failing::clean(&ndjson(OPENAI_EVENTS)),
        Dialect::Openai,
        None,
    );
    let text = read_all(&mut body);
    assert!(text.contains(r#""content":"hi""#), "{text}");
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
}

#[test]
fn a_stalled_stream_is_reported_in_band_not_as_a_truncation() {
    // The failure arrives after the role chunk. It must appear as an error
    // envelope: reporting it as a clean end would turn a truncated answer into
    // a successful turn.
    let mut body = body_of(
        Failing::then_fail(
            &ndjson(&[r#"{"type":"start","data":{}}"#]),
            StreamFailure::IdleTimeout { ms: 800 },
        ),
        Dialect::Openai,
        None,
    );
    let text = read_all(&mut body);
    assert!(
        text.contains("[idle-timeout] CC upstream idle timeout: no data for 800ms"),
        "{text}"
    );
    assert!(text.contains(r#""code":"network_error""#), "{text}");
    // No finish chunk: claiming a normal stop is what makes a client record a
    // failed turn as successful.
    assert!(!text.contains(r#""finish_reason":"stop""#), "{text}");
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
}

#[test]
fn a_failure_before_any_output_is_re_sent_invisibly() {
    // Nothing was delivered, so the replacement attempt is invisible: the
    // client sees only the successful stream, with no error at all.
    let replacement = ndjson(OPENAI_EVENTS);
    let first = ndjson(&[r#"{"type":"start","data":{}}"#]);
    let stream = UpstreamStream::new(
        Box::new(Failing::then_fail(&first, StreamFailure::ConnectionReset)),
        800,
    );
    let reconnect: Box<dyn FnMut() -> Result<UpstreamStream, StreamFailure> + Send> =
        Box::new(move || {
            Ok(UpstreamStream::new(
                Box::new(Failing::clean(&replacement)),
                800,
            ))
        });
    let mut body = SseBody::new(
        stream,
        Dialect::Openai,
        "m",
        Some(reconnect),
        SseBody::new_slot(),
    );
    let text = read_all(&mut body);
    assert!(!text.contains("error"), "no error record expected: {text}");
    assert!(text.contains(r#""content":"hi""#), "{text}");
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
}

/// A reader that produces nothing and always reports a read timeout, which is
/// how a socket that never sends a byte surfaces.
struct SilentUpstream;

impl Read for SilentUpstream {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "silent"))
    }
}

/// Build a body whose reconnect closure counts its calls, so the number of
/// re-sends can be asserted.
fn silent_then_ok_body(
    dialect: Dialect,
    retries: u64,
    calls: Arc<std::sync::atomic::AtomicU32>,
) -> SseBody {
    let no_output = UpstreamStream::with_tick(Box::new(SilentUpstream), 5_000, 300, 100);
    let reconnect: Box<dyn FnMut() -> Result<UpstreamStream, StreamFailure> + Send> =
        Box::new(move || {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                // The first replacement is still silent — the retry must happen
                // again rather than give up.
                Ok(UpstreamStream::with_tick(
                    Box::new(SilentUpstream),
                    5_000,
                    300,
                    100,
                ))
            } else {
                Ok(UpstreamStream::new(
                    Box::new(Failing::clean(&ndjson(OPENAI_EVENTS))),
                    800,
                ))
            }
        });
    SseBody::with_no_output_retries(
        no_output,
        dialect,
        "m",
        Some(reconnect),
        SseBody::new_slot(),
        None,
        retries,
    )
}

/// A stream that goes silent before producing anything is re-sent from scratch,
/// and — unlike the single-splice path — more than once: the retry is bounded by
/// the configured budget, not by a one-shot flag.
#[test]
fn a_silent_start_is_re_sent_from_scratch_more_than_once() {
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut body = silent_then_ok_body(Dialect::Openai, 3, Arc::clone(&calls));
    let text = read_all(&mut body);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the second re-send is what makes this a budget, not a one-shot"
    );
    assert!(text.contains(r#""content":"hi""#), "{text}");
    assert!(
        !text.contains("[no-output]"),
        "a recovered stream must not report the failure: {text}"
    );
}

#[test]
fn the_no_output_retry_budget_is_respected() {
    // Budget exhausted: every attempt is silent, so after `retries` re-sends the
    // client is told. With a budget of 1 that is exactly one re-send.
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let silent = UpstreamStream::with_tick(Box::new(SilentUpstream), 5_000, 300, 100);
    let counter = Arc::clone(&calls);
    let reconnect: Box<dyn FnMut() -> Result<UpstreamStream, StreamFailure> + Send> =
        Box::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(UpstreamStream::with_tick(
                Box::new(SilentUpstream),
                5_000,
                300,
                100,
            ))
        });
    let mut body = SseBody::with_no_output_retries(
        silent,
        Dialect::Openai,
        "m",
        Some(reconnect),
        SseBody::new_slot(),
        None,
        1,
    );
    let text = read_all(&mut body);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(text.contains("[no-output]"), "{text}");
}

#[test]
fn a_zero_budget_never_re_sends() {
    // CC_NO_OUTPUT_RETRIES=0 disables the mechanism for a caller that does not
    // want it, and must not call reconnect at all.
    let silent = UpstreamStream::with_tick(Box::new(SilentUpstream), 5_000, 300, 100);
    let reconnect: Box<dyn FnMut() -> Result<UpstreamStream, StreamFailure> + Send> =
        Box::new(|| -> Result<UpstreamStream, StreamFailure> {
            panic!("a zero budget must not re-send")
        });
    let mut body = SseBody::with_no_output_retries(
        silent,
        Dialect::Openai,
        "m",
        Some(reconnect),
        SseBody::new_slot(),
        None,
        0,
    );
    let text = read_all(&mut body);
    assert!(text.contains("[no-output]"), "{text}");
}

#[test]
fn a_failure_after_content_is_not_retried() {
    // Delivered bytes cannot be taken back, so a second attempt would duplicate
    // them. The failure is reported instead.
    let events = ndjson(&[
        r#"{"type":"start","data":{}}"#,
        r#"{"type":"text-delta","data":{"text":"partial"}}"#,
    ]);
    let stream = UpstreamStream::new(
        Box::new(Failing::then_fail(&events, StreamFailure::ConnectionReset)),
        800,
    );
    let reconnect: Box<dyn FnMut() -> Result<UpstreamStream, StreamFailure> + Send> =
        Box::new(|| -> Result<UpstreamStream, StreamFailure> {
            panic!("must not be called once content has been delivered")
        });
    let mut body = SseBody::new(
        stream,
        Dialect::Openai,
        "m",
        Some(reconnect),
        SseBody::new_slot(),
    );
    let text = read_all(&mut body);
    assert!(text.contains("partial"), "{text}");
    assert!(text.contains("[connection-reset] terminated"), "{text}");
}

#[test]
fn an_anthropic_failure_never_launders_itself_into_a_successful_turn() {
    // The defect this guards: a message_delta with stop_reason "end_turn" after
    // an error makes the client record a successful turn. The terminal records
    // for a failure are error + message_stop, with no message_delta between them.
    let events = ndjson(&[
        r#"{"type":"start","data":{}}"#,
        r#"{"type":"text-delta","data":{"text":"partial"}}"#,
    ]);
    let stream = UpstreamStream::new(
        Box::new(Failing::then_fail(&events, StreamFailure::ConnectionReset)),
        800,
    );
    let mut body = SseBody::new(stream, Dialect::Anthropic, "m", None, SseBody::new_slot());
    let text = read_all(&mut body);
    assert!(text.contains("event: error"), "{text}");
    assert!(text.contains("event: message_stop"), "{text}");
    assert!(
        !text.contains("message_delta"),
        "a message_delta after an error launders the failure: {text}"
    );
}

#[test]
fn a_slow_reader_receives_the_same_bytes_as_a_fast_one() {
    // tiny_http pulls the body in whatever chunk sizes the socket accepts, so
    // the body must not lose or re-split records when a read ends mid-buffer.
    // Each body mints its own response id, so the ids are masked before
    // comparing — the byte stream is what has to match.
    let events = ndjson(OPENAI_EVENTS);
    let mut whole = body_of(Failing::clean(&events), Dialect::Openai, None);
    let expected = mask_ids(&read_all(&mut whole));

    for bite in [1, 7, 64, 4096] {
        let mut body = body_of(Failing::clean(&events), Dialect::Openai, None);
        let got = mask_ids(&read_in_bites(&mut body, bite));
        assert_eq!(
            got, expected,
            "reading in {bite}-byte bites changed the output"
        );
    }
}

/// Replace generated ids and timestamps with placeholders, so two bodies can be
/// compared on structure alone.
fn mask_ids(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        let masked = match body.strip_prefix("data: ") {
            Some(json) if json.starts_with('{') => {
                match serde_json::from_str::<serde_json::Value>(json) {
                    Ok(mut v) => {
                        if let Some(obj) = v.as_object_mut() {
                            obj.remove("id");
                            obj.remove("created");
                        }
                        format!("data: {v}\n")
                    }
                    Err(_) => line.to_string(),
                }
            }
            _ => line.to_string(),
        };
        out.push_str(&masked);
    }
    out
}

#[test]
fn an_empty_upstream_still_terminates_the_stream() {
    // A body with no events at all must still produce a well-formed ending;
    // otherwise the client waits on a stream that will never close.
    let mut body = body_of(Failing::clean(""), Dialect::Openai, None);
    let text = read_all(&mut body);
    assert!(text.contains(r#""finish_reason":"stop""#), "{text}");
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");

    let mut body = body_of(Failing::clean(""), Dialect::Anthropic, None);
    let text = read_all(&mut body);
    assert!(text.contains("event: message_start"), "{text}");
    assert!(text.contains("event: message_stop"), "{text}");
}

#[test]
fn a_repeated_read_after_the_end_reports_eof() {
    let mut body = body_of(
        Failing::clean(&ndjson(OPENAI_EVENTS)),
        Dialect::Openai,
        None,
    );
    let _ = read_all(&mut body);
    let mut buf = [0u8; 16];
    assert_eq!(body.read(&mut buf).expect("eof read"), 0);
    assert_eq!(body.read(&mut buf).expect("eof read"), 0);
}

#[test]
fn the_encoders_stay_available_for_usage_after_the_body_is_drained() {
    // The usage slot is the only thing the caller can read afterwards: the
    // encoders are inside the body, which `respond` consumes.
    let events = ndjson(&[
        r#"{"type":"start","data":{}}"#,
        r#"{"type":"text-delta","data":{"text":"hi"}}"#,
        r#"{"type":"finish","data":{"finishReason":"stop","totalUsage":{"promptTokens":100,"completionTokens":5,"cachedInputTokens":64}}}"#,
    ]);
    let slot = SseBody::new_slot();
    let stream = UpstreamStream::new(Box::new(Failing::clean(&events)), 800);
    let mut body = SseBody::new(stream, Dialect::Openai, "m", None, slot.clone());
    let _ = read_all(&mut body);
    let observed = slot.lock().expect("slot").clone().expect("usage observed");
    assert_eq!(observed.prompt_tokens, Some(100));
    assert_eq!(observed.completion_tokens, Some(5));
    assert_eq!(observed.cached_tokens, Some(64));
}

#[test]
fn the_openai_encoder_reports_the_same_records_the_body_writes() {
    // Guards against the body and the encoder diverging on what "an event"
    // produces: the body is a thin wrapper, and this pins the wrapper.
    let mut body = body_of(
        Failing::clean(&ndjson(OPENAI_EVENTS)),
        Dialect::Openai,
        None,
    );
    let text = read_all(&mut body);

    let mut encoder = OpenAIEncoder::new("m");
    let mut expected = String::new();
    for line in ndjson(OPENAI_EVENTS).lines() {
        let json: serde_json::Value =
            serde_json::from_str(line.strip_prefix("data: ").expect("prefix")).expect("json");
        let kind = json["type"].as_str().expect("type");
        for chunk in encoder.emit(kind, &json["data"]).expect("emit") {
            expected.push_str(&ccproxy::sse::format_sse(&chunk));
        }
    }
    for chunk in encoder.terminal(None) {
        expected.push_str(&ccproxy::sse::format_sse(&chunk));
    }
    expected.push_str(&ccproxy::sse::format_sse_done());
    // The ids are generated per encoder, so compare the record structure only.
    let strip = |s: &str| {
        s.split(
            "

",
        )
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            let data = block
                .lines()
                .find_map(|l| l.strip_prefix("data: "))
                .unwrap_or("null");
            let mut v: serde_json::Value =
                serde_json::from_str(data).unwrap_or(serde_json::Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.remove("id");
                obj.remove("created");
            }
            v.to_string()
        })
        .collect::<Vec<_>>()
    };
    assert_eq!(strip(&text), strip(&expected));
    let _ = AnthropicEncoder::new("m");
}
