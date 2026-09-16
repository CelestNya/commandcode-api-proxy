//! Terminal shaping shared by the streaming and non-streaming paths.
//!
//! These functions used to live as private copies in `nonstream.rs`,
//! `openai_stream.rs` and `anthropic_stream.rs` — three files holding the same
//! logic with subtly different edge cases (the OpenAI stream copy dropped
//! degenerate tool arguments behind a `truthy` gate). One copy here makes the
//! two paths of each dialect agree by construction; the parity tests in
//! `crates/ccproxy/tests/terminal_parity.rs` pin that agreement.

use crate::usage::UsageData;
use serde_json::{json, Map, Value};

/// Maps CC's `finishReason` to OpenAI's `finish_reason`. Anything unknown
/// becomes "stop": reporting an unrecognised reason as a failure would be worse
/// than reporting a plain clean stop.
pub(crate) fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "stop" => "stop",
        "length" => "length",
        "content_filtered" => "content_filter",
        "tool-call" | "tool-calls" | "tool_call" => "tool_calls",
        "error" => "stop",
        _ => "stop",
    }
}

/// Maps CC's `finishReason` to Anthropic's `stop_reason`.
pub(crate) fn map_stop_reason(reason: &str) -> &'static str {
    match reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool-call" | "tool-calls" | "tool_call" => "tool_use",
        "content_filtered" => "stop_sequence",
        "pause_turn" => "pause_turn",
        "refusal" => "refusal",
        "model_context_window_exceeded" => "model_context_window_exceeded",
        _ => "end_turn",
    }
}

/// Required on every thinking block by the Anthropic contract. CC returns no
/// signature of its own, and clients round-trip the value without verifying it,
/// so a fixed placeholder is safe.
pub(crate) const THINKING_SIGNATURE: &str = "_cc_proxy_placeholder";

/// Canonical tool arguments as text: a string is used verbatim, any other
/// value is serialised, and a missing value becomes "".
pub(crate) fn canonical_arguments_text(data: &Value) -> String {
    let raw = data.get("input").or_else(|| data.get("arguments"));
    match raw {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// The human-readable message from an in-band error event.
pub(crate) fn error_message(data: &Value) -> String {
    if let Some(m) = data.get("message").and_then(Value::as_str) {
        return m.to_string();
    }
    if let Some(m) = data.pointer("/error/message").and_then(Value::as_str) {
        return m.to_string();
    }
    serde_json::to_string(data).unwrap_or_default()
}

/// The OpenAI `usage` object shared by the streaming and non-streaming paths.
pub(crate) fn openai_usage(usage: &UsageData) -> Value {
    let prompt = usage.prompt_tokens.unwrap_or(0);
    let completion = usage.completion_tokens.unwrap_or(0);
    let mut out = Map::new();
    out.insert("prompt_tokens".into(), json!(prompt));
    out.insert("completion_tokens".into(), json!(completion));
    out.insert(
        "total_tokens".into(),
        json!(usage
            .total_tokens
            .unwrap_or(prompt.saturating_add(completion))),
    );
    if let Some(cached) = usage.cached_tokens {
        out.insert(
            "prompt_tokens_details".into(),
            json!({"cached_tokens": cached}),
        );
    }
    if let Some(reasoning) = usage.reasoning_tokens {
        out.insert(
            "completion_tokens_details".into(),
            json!({"reasoning_tokens": reasoning}),
        );
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_reason_mapping_covers_the_contract() {
        assert_eq!(map_finish_reason("stop"), "stop");
        assert_eq!(map_finish_reason("length"), "length");
        assert_eq!(map_finish_reason("content_filtered"), "content_filter");
        assert_eq!(map_finish_reason("tool-call"), "tool_calls");
        assert_eq!(map_finish_reason("tool-calls"), "tool_calls");
        assert_eq!(map_finish_reason("error"), "stop");
        assert_eq!(map_finish_reason("anything"), "stop");
    }

    #[test]
    fn stop_reason_mapping_covers_the_contract() {
        assert_eq!(map_stop_reason("stop"), "end_turn");
        assert_eq!(map_stop_reason("length"), "max_tokens");
        assert_eq!(map_stop_reason("tool-call"), "tool_use");
        assert_eq!(map_stop_reason("content_filtered"), "stop_sequence");
        assert_eq!(map_stop_reason("pause_turn"), "pause_turn");
        assert_eq!(map_stop_reason("refusal"), "refusal");
        assert_eq!(
            map_stop_reason("model_context_window_exceeded"),
            "model_context_window_exceeded"
        );
        assert_eq!(map_stop_reason("anything"), "end_turn");
    }

    #[test]
    fn canonical_arguments_serialise_every_non_null_value() {
        assert_eq!(canonical_arguments_text(&json!({"input": "raw"})), "raw");
        assert_eq!(canonical_arguments_text(&json!({"input": false})), "false");
        assert_eq!(canonical_arguments_text(&json!({"input": 0})), "0");
        assert_eq!(
            canonical_arguments_text(&json!({"input": {"a": 1}})),
            "{\"a\":1}"
        );
        assert_eq!(canonical_arguments_text(&json!({"input": null})), "");
        assert_eq!(canonical_arguments_text(&json!({})), "");
        // The `arguments` alias is honoured too.
        assert_eq!(canonical_arguments_text(&json!({"arguments": "x"})), "x");
    }

    #[test]
    fn error_message_prefers_message_then_error_message() {
        assert_eq!(error_message(&json!({"message": "hi"})), "hi");
        assert_eq!(error_message(&json!({"error": {"message": "deep"}})), "deep");
        assert_eq!(error_message(&json!({"other": 1})), "{\"other\":1}");
    }

    #[test]
    fn openai_usage_fills_defaults_and_details() {
        let usage = UsageData {
            prompt_tokens: Some(10),
            completion_tokens: Some(20),
            total_tokens: None,
            cached_tokens: Some(4),
            reasoning_tokens: Some(2),
        };
        let v = openai_usage(&usage);
        assert_eq!(v["prompt_tokens"], 10);
        assert_eq!(v["completion_tokens"], 20);
        assert_eq!(v["total_tokens"], 30);
        assert_eq!(v["prompt_tokens_details"]["cached_tokens"], 4);
        assert_eq!(v["completion_tokens_details"]["reasoning_tokens"], 2);
    }
}
