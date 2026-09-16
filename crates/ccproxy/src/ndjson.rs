//! NDJSON line parsing for the CC upstream stream, ported from `parseCCLine`
//! in src/stream.ts.
//!
//! The upstream sends JSON lines, usually prefixed with `data: `. Anything
//! that is not a recognised event is a keepalive: unparseable lines, blank
//! lines, and bare `:` comments are all silently ignored rather than treated as
//! errors, because the upstream emits them as padding on long turns.

use serde_json::{Map, Value};

/// One parsed line.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedChunk {
    /// A CC event with its type and payload.
    Event(CCEvent),
    /// The `[DONE]` sentinel.
    Done,
    /// A keepalive or blank line: nothing to emit, not an error.
    Ping,
    /// Well-formed JSON that is not an event (no `type` field).
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CCEvent {
    pub kind: String,
    pub data: Value,
}

/// Parse one line of the upstream NDJSON stream.
pub fn parse_cc_line(line: &str) -> ParsedChunk {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return ParsedChunk::Ping;
    }

    // The prefix is exactly six characters including the space; a line reading
    // `data:{}` (no space) keeps its `data:` as part of the JSON and fails to
    // parse, which is the recorded behaviour.
    let data_str = match trimmed.strip_prefix("data: ") {
        Some(rest) => rest,
        None => trimmed,
    };

    if data_str == "[DONE]" {
        return ParsedChunk::Done;
    }

    let Ok(parsed) = serde_json::from_str::<Value>(data_str) else {
        // Not JSON — a keepalive, not an error.
        return ParsedChunk::Ping;
    };

    // Two shapes are in the wild: flat (`{type, id, text}`) and nested
    // (`{type, data:{text}}`). When `data` is an object it wins; otherwise the
    // remaining flat fields become the payload. Either way the top-level `id`
    // is dropped, since the downstream id is generated per request.
    let Some(obj) = parsed.as_object() else {
        return ParsedChunk::Unknown;
    };
    let Some(kind) = obj.get("type").and_then(Value::as_str) else {
        return ParsedChunk::Unknown;
    };

    let data = match obj.get("data") {
        Some(Value::Object(nested)) => Value::Object(nested.clone()),
        _ => {
            let mut rest = Map::new();
            for (key, value) in obj {
                if key == "type" || key == "id" {
                    continue;
                }
                rest.insert(key.clone(), value.clone());
            }
            Value::Object(rest)
        }
    };

    ParsedChunk::Event(CCEvent {
        kind: kind.to_string(),
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(line: &str) -> CCEvent {
        match parse_cc_line(line) {
            ParsedChunk::Event(e) => e,
            other => panic!("expected an event, got {other:?}"),
        }
    }

    #[test]
    fn blank_and_keepalive_lines_are_pings() {
        for line in ["", "   ", "\t", ":", ": keepalive"] {
            assert_eq!(parse_cc_line(line), ParsedChunk::Ping, "line {line:?}");
        }
    }

    #[test]
    fn unparseable_lines_are_pings_not_errors() {
        for line in ["data: {{{not json", "data: also-not-json", "not json"] {
            assert_eq!(parse_cc_line(line), ParsedChunk::Ping, "line {line:?}");
        }
    }

    #[test]
    fn parseable_json_without_a_type_is_unknown() {
        // Distinct from a ping: the line was valid JSON, it just is not an
        // event. `data: {}` is the case a keepalive-shaped read would get wrong.
        assert_eq!(parse_cc_line("data: {}"), ParsedChunk::Unknown);
    }

    #[test]
    fn done_marker_is_recognised_with_and_without_the_prefix() {
        assert_eq!(parse_cc_line("data: [DONE]"), ParsedChunk::Done);
        // The prefix is stripped before the comparison, so a bare [DONE] on its
        // own line is the same sentinel.
        assert_eq!(parse_cc_line("[DONE]"), ParsedChunk::Done);
    }

    #[test]
    fn the_data_prefix_is_exactly_six_characters() {
        // `data:{}` keeps the prefix in the JSON and therefore fails to parse:
        // the space is part of the contract, and this pins that it was never
        // tightened into a trim.
        assert_eq!(
            parse_cc_line("data:{\"type\":\"start\"}"),
            ParsedChunk::Ping
        );
        assert_eq!(event("data: {\"type\":\"start\"}").kind, "start");
    }

    #[test]
    fn nested_payloads_win_over_flat_fields() {
        let e = event(r#"data: {"type":"text-delta","id":"txt-0","data":{"text":"hi"}}"#);
        assert_eq!(e.kind, "text-delta");
        assert_eq!(e.data, json!({"text": "hi"}));
    }

    #[test]
    fn flat_payloads_keep_every_field_except_type_and_id() {
        let e = event(r#"data: {"type":"text-delta","id":"txt-0","text":"4"}"#);
        assert_eq!(e.kind, "text-delta");
        // The top-level id is dropped: the downstream id is generated locally.
        assert_eq!(e.data, json!({"text": "4"}));
    }

    #[test]
    fn json_without_a_type_is_unknown() {
        assert_eq!(parse_cc_line("data: {\"foo\":1}"), ParsedChunk::Unknown);
        assert_eq!(parse_cc_line("data: [1,2,3]"), ParsedChunk::Unknown);
        assert_eq!(parse_cc_line("data: 42"), ParsedChunk::Unknown);
    }
}
