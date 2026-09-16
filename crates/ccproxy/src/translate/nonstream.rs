//! Collapse a finished CC event stream into a single non-streaming response,
//! ported from `buildNonStreamingResponse` (OpenAI) and `buildAnthropicResponse`.
//!
//! Both throw on an in-band error rather than folding the failure into content:
//! a failed generation must not read as an assistant reply.

use crate::stream_body::Dialect;
use crate::translate::terminal::{
    canonical_arguments_text, map_finish_reason, map_stop_reason, openai_usage, THINKING_SIGNATURE,
};
use crate::translate::util::extract_usage;
use crate::usage::UsageData;
use serde_json::{json, Map, Value};

/// A tool call assembled from deltas or replaced wholesale by the canonical
/// event. Kept as a struct rather than a `Value` so accumulating into it cannot
/// go through an index that might not exist.
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// A generation that reported failure. The caller maps this to a 502 envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct GenerationFailed;

/// A collapsed non-streaming response plus the usage the caller records.
pub struct Collected {
    pub body: Value,
    pub usage: Option<UsageData>,
}

/// Track the fields the non-streaming builders need as events arrive, so the
/// whole stream never has to be buffered.
#[derive(Default)]
pub struct NonStreamingCollector {
    events: Vec<(String, Value)>,
}

impl NonStreamingCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, kind: &str, data: &Value) {
        self.events.push((kind.to_string(), data.clone()));
    }

    pub fn from_events(events: Vec<(String, Value)>) -> Self {
        Self { events }
    }

    /// Collapse the collected events into the response `dialect` expects. `Err`
    /// means the generation failed; the caller maps that to a 502 envelope.
    pub fn response(
        &self,
        dialect: Dialect,
        model: &str,
        id: &str,
    ) -> Result<Collected, GenerationFailed> {
        match dialect {
            Dialect::Openai => self.openai_response(model, id),
            Dialect::Anthropic => self.anthropic_response(model, id),
        }
    }

    /// OpenAI `chat.completion`. `Err` means the generation failed.
    pub fn openai_response(&self, model: &str, id: &str) -> Result<Collected, GenerationFailed> {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut finish: Option<(String, Option<UsageData>)> = None;

        for (kind, data) in &self.events {
            match kind.as_str() {
                "error" => return Err(GenerationFailed),
                "text-delta" => {
                    content.push_str(data.get("text").and_then(Value::as_str).unwrap_or(""));
                }
                "reasoning-delta" => {
                    reasoning.push_str(data.get("text").and_then(Value::as_str).unwrap_or(""));
                }
                "tool-call-delta" => {
                    let id = data.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                    let args = data.get("arguments").and_then(Value::as_str).unwrap_or("");
                    match calls.iter_mut().find(|tc| tc.id == id) {
                        Some(existing) => existing.arguments.push_str(args),
                        None => calls.push(ToolCall {
                            id: id.to_string(),
                            name: data
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            arguments: args.to_string(),
                        }),
                    }
                }
                "tool-call" => {
                    // A bridge commonly sends deltas then the canonical call for
                    // the same id; the canonical payload replaces the assembled
                    // one rather than appending to it.
                    let id = data.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                    let name = data
                        .get("toolName")
                        .or_else(|| data.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let entry = ToolCall {
                        id: id.to_string(),
                        name: name.to_string(),
                        arguments: canonical_arguments_text(data),
                    };
                    match calls.iter_mut().find(|tc| tc.id == id) {
                        Some(existing) => *existing = entry,
                        None => calls.push(entry),
                    }
                }
                "finish" => {
                    finish = Some((
                        data.get("finishReason")
                            .and_then(Value::as_str)
                            .unwrap_or("stop")
                            .to_string(),
                        extract_usage(data),
                    ));
                }
                _ => {}
            }
        }

        let mut message = Map::new();
        message.insert("role".into(), Value::String("assistant".into()));
        if !reasoning.is_empty() {
            message.insert("reasoning_content".into(), Value::String(reasoning));
        }
        if !calls.is_empty() {
            message.insert(
                "tool_calls".into(),
                Value::Array(
                    calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {"name": tc.name, "arguments": tc.arguments},
                            })
                        })
                        .collect(),
                ),
            );
        }
        if !content.is_empty() {
            message.insert("content".into(), Value::String(content));
        } else if calls.is_empty() {
            // Anthropic/OpenAI both require the field; an empty turn reports an
            // empty string rather than omitting it.
            message.insert("content".into(), Value::String(String::new()));
        }

        let reason = finish.as_ref().map(|(r, _)| r.as_str()).unwrap_or("stop");
        let mut response = Map::new();
        response.insert("id".into(), Value::String(id.to_string()));
        response.insert("object".into(), Value::String("chat.completion".into()));
        response.insert("created".into(), json!(crate::now_epoch_secs()));
        response.insert("model".into(), Value::String(model.to_string()));
        response.insert(
            "choices".into(),
            json!([{
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": map_finish_reason(reason),
            }]),
        );
        if let Some(usage) = finish.as_ref().and_then(|(_, u)| u.as_ref()) {
            response.insert("usage".into(), openai_usage(usage));
        }

        Ok(Collected {
            body: Value::Object(response),
            usage: finish.and_then(|(_, u)| u),
        })
    }

    /// Anthropic `message`.
    pub fn anthropic_response(
        &self,
        model: &str,
        message_id: &str,
    ) -> Result<Collected, GenerationFailed> {
        let mut text = String::new();
        let mut thinking = String::new();
        let mut tool_use: Vec<Value> = Vec::new();
        let mut finish: Option<(String, Option<UsageData>)> = None;

        for (kind, data) in &self.events {
            match kind.as_str() {
                "error" => return Err(GenerationFailed),
                "text-delta" => {
                    text.push_str(data.get("text").and_then(Value::as_str).unwrap_or(""));
                }
                "reasoning-delta" => {
                    thinking.push_str(data.get("text").and_then(Value::as_str).unwrap_or(""));
                }
                "tool-call" => {
                    let input = data
                        .get("input")
                        .or_else(|| data.get("arguments"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    tool_use.push(json!({
                        "type": "tool_use",
                        "id": data.get("toolCallId").and_then(Value::as_str).unwrap_or(""),
                        "name": data
                            .get("toolName")
                            .or_else(|| data.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        // Non-object inputs collapse to {}: Anthropic's contract
                        // requires an object and CC may send a bare string for an
                        // empty argument list.
                        "input": if input.is_object() { input } else { json!({}) },
                    }));
                }
                "finish" => {
                    finish = Some((
                        data.get("finishReason")
                            .and_then(Value::as_str)
                            .unwrap_or("stop")
                            .to_string(),
                        extract_usage(data),
                    ));
                }
                _ => {}
            }
        }

        // Thinking blocks precede the text they reason about and tool_use blocks
        // come last: strict clients use thinking-block position to continue
        // reasoning across turns.
        let mut content: Vec<Value> = Vec::new();
        if !thinking.is_empty() {
            content.push(json!({
                "type": "thinking",
                "thinking": thinking,
                "signature": THINKING_SIGNATURE,
            }));
        }
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
        content.extend(tool_use);
        if content.is_empty() {
            // Anthropic requires a non-empty array; an empty refusal still needs
            // a block.
            content.push(json!({"type": "text", "text": ""}));
        }

        let reason = finish.as_ref().map(|(r, _)| r.as_str()).unwrap_or("stop");
        let usage = finish.as_ref().and_then(|(_, u)| u.as_ref());
        let response = json!({
            "id": message_id,
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": content,
            "stop_reason": map_stop_reason(reason),
            "stop_sequence": null,
            "usage": {
                "input_tokens": usage.and_then(|u| u.prompt_tokens).unwrap_or(0),
                "output_tokens": usage.and_then(|u| u.completion_tokens).unwrap_or(0),
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": usage.and_then(|u| u.cached_tokens).unwrap_or(0),
            },
        });

        Ok(Collected {
            body: response,
            usage: finish.and_then(|(_, u)| u),
        })
    }
}
