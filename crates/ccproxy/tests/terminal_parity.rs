//! Parity between the streaming and non-streaming paths of the same dialect.
//!
//! The terminal-shaping helpers (finish-reason maps, canonical tool arguments,
//! error-message extraction, usage assembly) used to live in three files as
//! subtly different copies. This test pins the behaviour the two paths of each
//! dialect must share, so the shared `translate::terminal` module cannot
//! silently drift again — the drift that motivated it is test 1 below.

use ccproxy::translate::{AnthropicEncoder, NonStreamingCollector, OpenAIEncoder};
use serde_json::{json, Value};

const MODEL: &str = "parity-test";

/// The tool-call `function.arguments` the OpenAI non-streaming path produces.
fn non_stream_openai_tool_arguments(data: &Value) -> String {
    let mut collector = NonStreamingCollector::new();
    collector.push("tool-call", data);
    collector.push("finish", &json!({"finishReason": "stop"}));
    let collected = collector.openai_response(MODEL, "id_1").expect("collects");
    collected
        .body
        .pointer("/choices/0/message/tool_calls/0/function/arguments")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The tool-call `function.arguments` the OpenAI streaming path produces.
fn stream_openai_tool_arguments(data: &Value) -> String {
    let mut encoder = OpenAIEncoder::new(MODEL);
    let chunks = encoder.emit("tool-call", data).expect("emits");
    let mut out = String::new();
    for chunk in chunks {
        if let Some(args) = chunk
            .pointer("/choices/0/delta/tool_calls/0/function/arguments")
            .and_then(Value::as_str)
        {
            out = args.to_string();
        }
    }
    out
}

/// The `finish_reason` the OpenAI non-streaming path produces.
fn non_stream_openai_finish_reason(reason: &str) -> String {
    let mut collector = NonStreamingCollector::new();
    collector.push("finish", &json!({"finishReason": reason}));
    collector
        .openai_response(MODEL, "id_1")
        .expect("collects")
        .body
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The `finish_reason` the OpenAI streaming path produces.
fn stream_openai_finish_reason(reason: &str) -> String {
    let mut encoder = OpenAIEncoder::new(MODEL);
    let chunks = encoder
        .emit("finish", &json!({"finishReason": reason}))
        .expect("emits");
    let mut out = String::new();
    for chunk in chunks {
        if let Some(r) = chunk
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
        {
            out = r.to_string();
        }
    }
    out
}

/// The `stop_reason` the Anthropic non-streaming path produces.
fn non_stream_anthropic_stop_reason(reason: &str) -> String {
    let mut collector = NonStreamingCollector::new();
    collector.push("finish", &json!({"finishReason": reason}));
    collector
        .anthropic_response(MODEL, "msg_1")
        .expect("collects")
        .body
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The `stop_reason` the Anthropic streaming path produces.
fn stream_anthropic_stop_reason(reason: &str) -> String {
    let mut encoder = AnthropicEncoder::new(MODEL);
    let records = encoder
        .emit("finish", &json!({"finishReason": reason}))
        .expect("emits");
    let mut out = String::new();
    for record in &records {
        if record.event == "message_delta" {
            if let Some(r) = record
                .data
                .pointer("/delta/stop_reason")
                .and_then(Value::as_str)
            {
                out = r.to_string();
            }
        }
    }
    out
}

#[test]
fn openai_stream_and_non_stream_agree_on_canonical_tool_arguments() {
    // A bare `false` is a degenerate but legal tool input. Both paths must
    // serialise it the same way; the streaming copy used to drop it behind a
    // `truthy` gate while the non-streaming copy serialised it verbatim.
    let tool_call = json!({"toolCallId": "call_1", "toolName": "get_weather", "input": false});
    let stream = stream_openai_tool_arguments(&tool_call);
    let non_stream = non_stream_openai_tool_arguments(&tool_call);
    assert_eq!(
        stream, non_stream,
        "stream={stream:?} non_stream={non_stream:?}"
    );
}

#[test]
fn openai_finish_reason_matches_between_stream_and_non_stream() {
    for reason in [
        "stop",
        "length",
        "content_filtered",
        "tool-call",
        "error",
        "mystery",
    ] {
        assert_eq!(
            stream_openai_finish_reason(reason),
            non_stream_openai_finish_reason(reason),
            "finishReason={reason:?}"
        );
    }
}

#[test]
fn anthropic_stop_reason_matches_between_stream_and_non_stream() {
    for reason in [
        "stop",
        "length",
        "tool-call",
        "content_filtered",
        "pause_turn",
        "refusal",
        "model_context_window_exceeded",
        "mystery",
    ] {
        assert_eq!(
            stream_anthropic_stop_reason(reason),
            non_stream_anthropic_stop_reason(reason),
            "finishReason={reason:?}"
        );
    }
}

#[test]
fn openai_usage_assembly_matches_between_stream_and_non_stream() {
    let finish = json!({
        "finishReason": "stop",
        "usage": {
            "promptTokens": 10,
            "completionTokens": 20,
            "totalTokens": 30,
            "cachedInputTokens": 4,
            "reasoningTokens": 2,
        },
    });

    let mut collector = NonStreamingCollector::new();
    collector.push("finish", &finish);
    let non_stream = collector
        .openai_response(MODEL, "id_1")
        .expect("collects")
        .body
        .get("usage")
        .cloned()
        .unwrap_or(Value::Null);

    let mut encoder = OpenAIEncoder::new(MODEL);
    let chunks = encoder.emit("finish", &finish).expect("emits");
    let mut stream = Value::Null;
    for chunk in chunks {
        if let Some(u) = chunk.get("usage") {
            stream = u.clone();
        }
    }
    assert_eq!(stream, non_stream);
}
