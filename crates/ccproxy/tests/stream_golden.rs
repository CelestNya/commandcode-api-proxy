//! Replays the recorded stream transcripts against the Rust encoders, with no
//! network involved.
//!
//! Inputs come from conformance/scenarios/upstream-scenarios.json (the canonical
//! upstream event sequences); expectations come from conformance/golden/
//! behaviour.json (what the Node proxy sent downstream). Between them they cover
//! every streaming case, including the terminal states that are hardest to
//! reach in a live test: truncated streams, in-band errors before and after
//! content, late events after finish, and keepalive garbage.
//!
//! Failure classes that only exist as transport errors (idle timeout,
//! connection reset) are driven through the encoder's own terminal API rather
//! than by simulating a socket: the encoder is what decides the record shapes.

use ccproxy::ndjson::{parse_cc_line, ParsedChunk};
use ccproxy::sse::{AnthropicRecord, StreamFailure};
use ccproxy::translate::{AnthropicEncoder, OpenAIEncoder};
use serde_json::Value;

const SCENARIOS: &str = include_str!("../../../conformance/scenarios/upstream-scenarios.json");
const GOLDEN: &str = include_str!("../../../conformance/golden/behaviour.json");

const MODEL: &str = "deepseek-v4-flash";

fn scenarios() -> Value {
    serde_json::from_str(SCENARIOS).expect("scenarios parse")
}

fn golden() -> Value {
    serde_json::from_str(GOLDEN).expect("behaviour golden parses")
}

fn case(name: &str) -> Value {
    golden()["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no case {name}"))
        .clone()
}

/// Feed a scenario's NDJSON through a parser, calling `emit` for each event.
/// Lines that are not events (pings, garbage) are skipped, exactly as the
/// upstream reader does.
fn feed(ndjson: &[Value], mut emit: impl FnMut(&str, &Value)) {
    for event in ndjson {
        let line = format!("data: {event}");
        if let ParsedChunk::Event(e) = parse_cc_line(&line) {
            emit(&e.kind, &e.data);
        }
    }
}

/// Normalise a record the way the recorder does: ids, timestamps and message
/// ids become placeholders, and object keys are sorted. Returns the same shape
/// the golden stores, so a mismatch prints a readable diff.
fn normalise(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(pin_varying_strings(s)),
        Value::Array(items) => Value::Array(items.iter().map(normalise).collect()),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                let v = &map[k];
                // `created` is a wall-clock timestamp; the contract is that the
                // field exists and is a number, not its value.
                if k == "created" {
                    out.insert(k.clone(), Value::String("<epoch>".into()));
                } else {
                    out.insert(k.clone(), normalise(v));
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Replace run-varying identifiers embedded in strings.
fn pin_varying_strings(s: &str) -> String {
    let is_uuid = |part: &str| {
        part.len() == 36
            && part.chars().enumerate().all(|(i, c)| match i {
                8 | 13 | 18 | 23 => c == '-',
                _ => c.is_ascii_hexdigit(),
            })
    };
    if is_uuid(s) {
        return "<uuid>".into();
    }
    // `msg_<uuid>` and any other id with a uuid suffix.
    if let Some((prefix, rest)) = s.rsplit_once('_') {
        if is_uuid(rest) {
            return format!("{prefix}_<uuid>");
        }
    }
    s.to_string()
}

/// Compare emitted records against the recorded ones for a case.
fn assert_records_match(case_name: &str, got: Vec<Value>) {
    let want = case(case_name)["downstream"]["records"].clone();
    let got = Value::Array(got.into_iter().map(|r| normalise(&r)).collect());
    let want = normalise(&want);
    assert_eq!(
        serde_json::to_string_pretty(&got).expect("serialise"),
        serde_json::to_string_pretty(&want).expect("serialise"),
        "{case_name}: downstream records differ"
    );
}

/// The upstream scripts a scenario defines, in the order they are served.
///
/// A scenario is either one script (`ndjson`) served to every request, or an
/// `attempts` list where each entry answers one request — that is what lets a
/// retry be recorded: attempt 1 can die mid-stream and attempt 2 succeed.
fn scripts(scenario: &Value) -> Vec<Vec<(String, Value)>> {
    let mut out = Vec::new();
    if let Some(attempts) = scenario["attempts"].as_array() {
        for attempt in attempts {
            out.push(parse_events(attempt["ndjson"].as_array()));
        }
    }
    if out.is_empty() {
        out.push(parse_events(scenario["ndjson"].as_array()));
    }
    out
}

fn parse_events(ndjson: Option<&Vec<Value>>) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    for event in ndjson.cloned().unwrap_or_default() {
        let line = format!("data: {event}");
        if let ParsedChunk::Event(e) = parse_cc_line(&line) {
            events.push((e.kind, e.data));
        }
    }
    events
}

/// Drive one scenario through the OpenAI encoder, mirroring the pump.
///
/// The pump allows exactly one replacement attempt, decided by
/// `can_splice_retry` — answer text or a tool call already sent blocks it,
/// reasoning alone does not, because the replacement's replayed reasoning is
/// dropped. A replacement **reuses the same encoder**: with nothing sent yet,
/// the retry's own opening role chunk is what the client sees, which is why a
/// recovered failure still shows the role chunk twice in the golden (clients
/// merge a repeated role delta, so this is invisible to them).
fn run_openai(scenario_name: &str, ending: Ending) -> Vec<Value> {
    let s = scenarios()["scenarios"][scenario_name].clone();
    let attempts = scripts(&s);
    // Beyond the scripted attempts the last one repeats, matching the mock:
    // a retry that should not have happened shows up as duplicate output
    // rather than as a hang.
    let script_for = |n: usize| -> &Vec<(String, Value)> {
        attempts
            .get(n.min(attempts.len().saturating_sub(1)))
            .unwrap_or(&attempts[0])
    };
    let mut attempt = script_for(0).clone();
    let mut served = 1usize;

    let mut encoder = OpenAIEncoder::new(MODEL);
    let mut out: Vec<Value> = Vec::new();
    let mut retried = false;

    loop {
        let mut failed: Option<StreamFailure> = None;
        for (kind, data) in &attempt {
            match encoder.emit(kind, data) {
                Ok(chunks) => out.extend(chunks),
                Err(failure) => {
                    failed = Some(failure);
                    break;
                }
            }
        }
        match failed {
            None => break,
            Some(failure) => {
                if retried || !encoder.can_splice_retry() {
                    // Report the failure and stop.
                    out.extend(encoder.stream_error_chunks(&failure));
                    out.push(Value::String("<DONE>".into()));
                    return out;
                }
                retried = true;
                encoder.begin_continuation();
                attempt = script_for(served).clone();
                served += 1;
            }
        }
    }

    match ending {
        Ending::Eof => {
            if !encoder.finished() {
                out.extend(encoder.finish_chunks("stop"));
            }
            out.push(Value::String("<DONE>".into()));
        }
        Ending::Failure(failure) => {
            // A transport failure arrives outside the event loop; the pump
            // applies the same retry rule to it.
            if !retried && encoder.can_splice_retry() {
                // Re-send, then fail again: the second failure is reported.
                encoder.begin_continuation();
                for (kind, data) in script_for(served) {
                    if let Ok(chunks) = encoder.emit(kind, data) {
                        out.extend(chunks);
                    }
                }
            }
            out.extend(encoder.stream_error_chunks(&failure));
            out.push(Value::String("<DONE>".into()));
        }
    }
    out
}

/// Anthropic counterpart of `run_openai`. The replacement attempt reuses the
/// same encoder, so a recovered failure still shows exactly one `message_start`
/// (the buffered `start` produces none of its own until content arrives).
fn run_anthropic(scenario_name: &str, ending: Ending) -> Vec<AnthropicRecord> {
    let s = scenarios()["scenarios"][scenario_name].clone();
    let attempts = scripts(&s);
    let script_for = |n: usize| -> &Vec<(String, Value)> {
        attempts
            .get(n.min(attempts.len().saturating_sub(1)))
            .unwrap_or(&attempts[0])
    };
    let mut attempt = script_for(0).clone();
    let mut served = 1usize;

    let mut encoder = AnthropicEncoder::new(MODEL);
    let mut out: Vec<AnthropicRecord> = Vec::new();
    let mut retried = false;

    loop {
        let mut failed: Option<StreamFailure> = None;
        for (kind, data) in &attempt {
            match encoder.emit(kind, data) {
                Ok(records) => out.extend(records),
                Err(failure) => {
                    failed = Some(failure);
                    break;
                }
            }
        }
        match failed {
            None => break,
            Some(failure) => {
                if retried || !encoder.can_splice_retry() {
                    out.extend(encoder.error_records(&failure, true));
                    return out;
                }
                retried = true;
                // The replacement stream replays its `start` and thinking; both
                // are already in the client's hands.
                encoder.begin_continuation();
                attempt = script_for(served).clone();
                served += 1;
            }
        }
    }

    match ending {
        // A stream that already finished must not get a second terminal pair.
        Ending::Eof => {
            if !encoder.finished() {
                out.extend(encoder.finish_records("end_turn"));
            }
        }
        Ending::Failure(failure) => {
            if !encoder.finished() {
                out.extend(encoder.error_records(&failure, true));
            }
        }
    }
    out
}

#[derive(Clone)]
enum Ending {
    Eof,
    Failure(StreamFailure),
}

/// The scenarios that end with a clean upstream EOF and need no synthesis.
const CLEAN: [&str; 8] = [
    "clean-text",
    "multi-delta",
    "reasoning-then-text",
    "tool-call-delta-then-final",
    "tool-call-final-only",
    "parallel-tool-calls",
    "late-event-after-finish",
    "length-finish-reason",
];

/// Scenarios whose upstream ends without a `finish` event.
const TRUNCATED: [&str; 4] = [
    "start-only-truncated",
    "text-without-finish",
    "empty-body",
    "garbage-lines",
];

fn records_to_json(records: &[AnthropicRecord]) -> Vec<Value> {
    records
        .iter()
        .map(|r| {
            let mut m = serde_json::Map::new();
            m.insert("event".into(), Value::String(r.event.into()));
            m.insert("data".into(), r.data.clone());
            Value::Object(m)
        })
        .collect()
}

/// Records from the OpenAI encoder, tagged the way the recorder tags them: SSE
/// data lines become `{event: "data", data: ...}`.
fn chunks_to_json(chunks: &[Value]) -> Vec<Value> {
    chunks
        .iter()
        .map(|c| {
            let mut m = serde_json::Map::new();
            m.insert("event".into(), Value::String("data".into()));
            m.insert("data".into(), c.clone());
            Value::Object(m)
        })
        .collect()
}

#[test]
fn openai_clean_streams_match() {
    for name in CLEAN {
        let records = chunks_to_json(&run_openai(name, Ending::Eof));
        assert_records_match(&format!("stream/openai/{name}"), records);
    }
}

#[test]
fn anthropic_clean_streams_match() {
    for name in CLEAN {
        let records = records_to_json(&run_anthropic(name, Ending::Eof));
        assert_records_match(&format!("stream/anthropic/{name}"), records);
    }
}

#[test]
fn openai_truncated_streams_match() {
    for name in TRUNCATED {
        let records = chunks_to_json(&run_openai(name, Ending::Eof));
        assert_records_match(&format!("stream/openai/{name}"), records);
    }
}

#[test]
fn anthropic_truncated_streams_match() {
    for name in TRUNCATED {
        let records = records_to_json(&run_anthropic(name, Ending::Eof));
        assert_records_match(&format!("stream/anthropic/{name}"), records);
    }
}

#[test]
fn openai_in_band_error_matches() {
    // `error-event` throws before any content, so the retry path is what the
    // client sees; the recorded transcript is the second attempt's output.
    let records = chunks_to_json(&run_openai("error-event", Ending::Eof));
    assert_records_match("stream/openai/error-event", records);

    let records = chunks_to_json(&run_openai("error-after-content", Ending::Eof));
    assert_records_match("stream/openai/error-after-content", records);
}

#[test]
fn anthropic_in_band_error_matches() {
    let records = records_to_json(&run_anthropic("error-event", Ending::Eof));
    assert_records_match("stream/anthropic/error-event", records);

    let records = records_to_json(&run_anthropic("error-after-content", Ending::Eof));
    assert_records_match("stream/anthropic/error-after-content", records);
}

/// The 2026-09-15 production shape: the upstream dies mid-stream after
/// reasoning but before any answer text.
///
/// Thinking already delivered must not block recovery (its replay is
/// suppressed), so the transcript shows two attempts spliced into one
/// continuous response — one `message_start`, the first attempt's thinking,
/// then the second attempt's answer. The old gate refused this and lost the
/// whole turn.
#[test]
fn retry_splice_recovers_after_reasoning_only() {
    let records = records_to_json(&run_anthropic(
        "error-after-reasoning-recovered",
        Ending::Eof,
    ));
    assert_records_match("stream/anthropic/error-after-reasoning-recovered", records);

    let chunks = chunks_to_json(&run_openai("error-after-reasoning-recovered", Ending::Eof));
    assert_records_match("stream/openai/error-after-reasoning-recovered", chunks);
}

/// Answer text already delivered: retrying would show the user the same
/// paragraph twice, so exactly one attempt is made and the failure is reported
/// after the partial answer.
#[test]
fn retry_is_refused_once_answer_text_exists() {
    let records = records_to_json(&run_anthropic("error-after-text-no-retry", Ending::Eof));
    assert_records_match("stream/anthropic/error-after-text-no-retry", records);

    let chunks = chunks_to_json(&run_openai("error-after-text-no-retry", Ending::Eof));
    assert_records_match("stream/openai/error-after-text-no-retry", chunks);
}

#[test]
fn openai_transport_failures_match() {
    let reset = StreamFailure::ConnectionReset;
    let records = chunks_to_json(&run_openai("reset-mid-stream", Ending::Failure(reset)));
    assert_records_match("stream/openai/reset-mid-stream", records);

    let idle = StreamFailure::IdleTimeout { ms: 800 };
    let records = chunks_to_json(&run_openai("hang-after-start", Ending::Failure(idle)));
    assert_records_match("stream/openai/hang-after-start", records);
}

#[test]
fn anthropic_transport_failures_match() {
    let reset = StreamFailure::ConnectionReset;
    let records = records_to_json(&run_anthropic("reset-mid-stream", Ending::Failure(reset)));
    assert_records_match("stream/anthropic/reset-mid-stream", records);

    let idle = StreamFailure::IdleTimeout { ms: 800 };
    let records = records_to_json(&run_anthropic("hang-after-start", Ending::Failure(idle)));
    assert_records_match("stream/anthropic/hang-after-start", records);
}

#[test]
fn every_stream_case_is_covered() {
    // Guard against a new scenario being added to the fixture without a test.
    let covered: Vec<String> = CLEAN
        .iter()
        .chain(TRUNCATED.iter())
        .chain(
            [
                "error-event",
                "error-after-content",
                "reset-mid-stream",
                "hang-after-start",
                "error-after-reasoning-recovered",
                "error-after-text-no-retry",
            ]
            .iter(),
        )
        .map(|s| s.to_string())
        .collect();
    let all: Vec<String> = scenarios()["scenarios"]
        .as_object()
        .expect("scenarios object")
        .keys()
        .cloned()
        .collect();
    for name in &all {
        assert!(
            covered.contains(name),
            "scenario {name} has no stream test; add it to CLEAN/TRUNCATED or its own test"
        );
    }
    assert_eq!(all.len(), covered.len(), "covered list has stale entries");
}

#[test]
fn openai_nonstreaming_responses_match() {
    for name in ["clean-text", "reasoning-then-text", "tool-call-final-only"] {
        let records = openai_nonstreaming(name);
        assert_records_match_body(&format!("nonstream/openai/{name}"), records);
    }
}

#[test]
fn anthropic_nonstreaming_responses_match() {
    for name in ["clean-text", "reasoning-then-text", "tool-call-final-only"] {
        let records = anthropic_nonstreaming(name);
        assert_records_match_body(&format!("nonstream/anthropic/{name}"), records);
    }
}

#[test]
fn nonstreaming_error_event_is_not_folded_into_content() {
    // A failed generation must surface as an error, never as an assistant
    // reply that reads like a normal answer.
    for name in ["openai", "anthropic"] {
        let failed = if name == "openai" {
            openai_nonstreaming_result("error-event").is_err()
        } else {
            anthropic_nonstreaming_result("error-event").is_err()
        };
        assert!(failed, "{name} non-streaming must reject an error event");
    }
}

/// Collect a scenario's events the way the server does for a non-streaming
/// request, then collapse them.
fn collect(name: &str) -> ccproxy::translate::nonstream::NonStreamingCollector {
    let s = scenarios()["scenarios"][name].clone();
    let mut collector = ccproxy::translate::nonstream::NonStreamingCollector::new();
    let ndjson = s["ndjson"].as_array().cloned().unwrap_or_default();
    feed(&ndjson, |kind, data| collector.push(kind, data));
    collector
}

fn openai_nonstreaming(name: &str) -> Value {
    openai_nonstreaming_result(name).expect("generation succeeds")
}

fn openai_nonstreaming_result(name: &str) -> Result<Value, ()> {
    let c = collect(name);
    // The OpenAI id is a bare UUID (the recorder pins it to <uuid>).
    c.openai_response(MODEL, "12345678-1234-1234-1234-123456789012")
        .map(|r| r.body)
        .map_err(|_| ())
}

fn anthropic_nonstreaming(name: &str) -> Value {
    anthropic_nonstreaming_result(name).expect("generation succeeds")
}

fn anthropic_nonstreaming_result(name: &str) -> Result<Value, ()> {
    let c = collect(name);
    c.anthropic_response(MODEL, "msg_12345678-1234-1234-1234-123456789012")
        .map(|r| r.body)
        .map_err(|_| ())
}

/// Compare a non-streaming body against the recorded one.
fn assert_records_match_body(case_name: &str, got: Value) {
    let want = normalise(&case(case_name)["downstream"]["body"]);
    let got = normalise(&got);
    assert_eq!(
        serde_json::to_string_pretty(&got).expect("serialise"),
        serde_json::to_string_pretty(&want).expect("serialise"),
        "{case_name}: downstream body differs"
    );
}

// ── hard constraints, asserted directly rather than via a scenario ──────────
//
// These hold across every case above, so a regression that only shows up on an
// unusual upstream shape is caught here rather than by luck.

#[test]
fn every_anthropic_record_carries_a_matching_type() {
    let mut checked = 0;
    for scenario in scenarios()["scenarios"]
        .as_object()
        .expect("scenarios")
        .keys()
    {
        for ending in [Ending::Eof, Ending::Failure(StreamFailure::ConnectionReset)] {
            for record in run_anthropic(scenario, ending.clone()) {
                assert_eq!(
                    record.data.get("type").and_then(Value::as_str),
                    Some(record.event),
                    "{scenario}: event name and payload type must agree"
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked > 50,
        "expected to have checked many records, got {checked}"
    );
}

#[test]
fn anthropic_streams_never_emit_a_top_level_signature_delta() {
    // A signature_delta is a *delta type*; as a top-level event it makes strict
    // clients abort the whole stream.
    for scenario in scenarios()["scenarios"]
        .as_object()
        .expect("scenarios")
        .keys()
    {
        for record in run_anthropic(scenario, Ending::Eof) {
            assert_ne!(record.event, "signature_delta", "{scenario}");
        }
    }
}

#[test]
fn openai_streams_end_with_exactly_one_done() {
    for scenario in scenarios()["scenarios"]
        .as_object()
        .expect("scenarios")
        .keys()
    {
        for records in [
            chunks_to_json(&run_openai(scenario, Ending::Eof)),
            chunks_to_json(&run_openai(
                scenario,
                Ending::Failure(StreamFailure::ClientGone),
            )),
        ] {
            let dones = records
                .iter()
                .filter(|r| r["data"] == Value::String("<DONE>".into()))
                .count();
            assert_eq!(dones, 1, "{scenario}: [DONE] must appear exactly once");
            assert_eq!(
                records.last().map(|r| r["data"].clone()),
                Some(Value::String("<DONE>".into())),
                "{scenario}: [DONE] must be last"
            );
        }
    }
}

#[test]
fn no_stream_emits_records_after_a_terminal_pair() {
    // The terminal guard: a misbehaving upstream that keeps sending after
    // finish must not produce a second terminal record or post-terminal content.
    let records = run_anthropic("late-event-after-finish", Ending::Eof);
    let stops = records.iter().filter(|r| r.event == "message_stop").count();
    assert_eq!(stops, 1, "message_stop must appear exactly once");
    assert_eq!(
        records.last().map(|r| r.event),
        Some("message_stop"),
        "nothing may follow message_stop"
    );

    let records = chunks_to_json(&run_openai("late-event-after-finish", Ending::Eof));
    let finishes = records
        .iter()
        .filter(|r| r["data"]["choices"][0]["finish_reason"] != Value::Null)
        .count();
    assert_eq!(finishes, 1, "exactly one finish_reason record");
}

#[test]
fn a_failure_after_content_never_reports_a_normal_stop() {
    // The laundering hazard: a trailing message_delta(end_turn) makes the client
    // record a successful turn for a generation that failed.
    for scenario in [
        "error-after-content",
        "reset-mid-stream",
        "hang-after-start",
    ] {
        let records = run_anthropic(scenario, Ending::Failure(StreamFailure::ConnectionReset));
        assert!(
            records.iter().any(|r| r.event == "error"),
            "{scenario}: the failure must be reported"
        );
        let last_delta = records.iter().rev().find(|r| r.event == "message_delta");
        assert!(
            last_delta.is_none(),
            "{scenario}: no message_delta may follow a failure (it would read as end_turn)"
        );
    }
}
