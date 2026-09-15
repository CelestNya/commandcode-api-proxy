//! CC events -> OpenAI `chat.completion.chunk` records, ported from
//! `OpenAIStreamEncoder` in src/translate/openai.ts.
//!
//! Two pieces of state carry the contract:
//!
//! * `emitted_content` distinguishes "nothing worth keeping went out" from
//!   "the client already has output". It gates the recovery retry: only a
//!   failure with no content can be re-sent invisibly. The opening role chunk
//!   deliberately does not count — clients merge a repeated role delta.
//! * `tool_call_id_to_index` assigns each tool-call id a stable streaming
//!   `index`. Without it, parallel tool-call deltas arriving without an
//!   explicit index would all land on 0 and the client would merge them into
//!   one call.

use crate::sse::StreamFailure;
use crate::tool_arguments::tool_argument_suffix;
use crate::translate::util::{extract_usage, truthy};
use crate::usage::UsageData;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// Maps CC's `finishReason` to OpenAI's `finish_reason`. Anything unknown
/// becomes "stop": reporting an unrecognised reason as a failure would be worse
/// than reporting a plain clean stop.
fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "stop" => "stop",
        "length" => "length",
        "content_filtered" => "content_filter",
        "tool-call" | "tool-calls" | "tool_call" => "tool_calls",
        "error" => "stop",
        _ => "stop",
    }
}

#[derive(Debug, Default, Clone)]
struct ToolMeta {
    id: Option<String>,
    name: Option<String>,
}

pub struct OpenAIEncoder {
    id: String,
    created: u64,
    model: String,
    tool_call_index: usize,
    saw_finish: bool,
    emitted_content: bool,
    pub last_usage: Option<UsageData>,
    tool_call_id_to_index: HashMap<String, usize>,
    tool_arguments: HashMap<usize, String>,
    tool_metadata: HashMap<usize, ToolMeta>,
}

impl OpenAIEncoder {
    pub fn new(model: &str) -> Self {
        Self {
            // A bare UUID, matching what the Node encoder generates.
            id: uuid::Uuid::new_v4().to_string(),
            created: crate::now_epoch_secs(),
            model: model.to_string(),
            tool_call_index: 0,
            saw_finish: false,
            emitted_content: false,
            last_usage: None,
            tool_call_id_to_index: HashMap::new(),
            tool_arguments: HashMap::new(),
            tool_metadata: HashMap::new(),
        }
    }

    /// The response id, reused for the non-streaming shape so both modes agree.
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn finished(&self) -> bool {
        self.saw_finish
    }

    /// Whether the client has received anything worth keeping. False means a
    /// failed attempt can be re-sent invisibly.
    pub fn has_emitted_content(&self) -> bool {
        self.emitted_content
    }

    fn envelope(&self, choices: Value) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), Value::String(self.id.clone()));
        m.insert(
            "object".into(),
            Value::String("chat.completion.chunk".into()),
        );
        m.insert("created".into(), json!(self.created));
        m.insert("model".into(), Value::String(self.model.clone()));
        m.insert("choices".into(), choices);
        Value::Object(m)
    }

    fn delta_chunk(&self, delta: Value) -> Value {
        self.envelope(json!([{"index": 0, "delta": delta, "finish_reason": null}]))
    }

    /// Resolve the streaming index for a tool-call id, allocating on first
    /// sighting. Falls back to the upstream-provided index when the upstream
    /// already numbers its calls.
    fn resolve_tool_call_index(
        &mut self,
        tool_call_id: Option<&str>,
        upstream_index: Option<u64>,
    ) -> usize {
        if let Some(id) = tool_call_id {
            if let Some(known) = self.tool_call_id_to_index.get(id) {
                return *known;
            }
            let idx = match upstream_index {
                Some(i) => i as usize,
                None => {
                    let i = self.tool_call_index;
                    self.tool_call_index = self.tool_call_index.saturating_add(1);
                    i
                }
            };
            self.tool_call_id_to_index.insert(id.to_string(), idx);
            return idx;
        }
        match upstream_index {
            Some(i) => i as usize,
            None => {
                let i = self.tool_call_index;
                self.tool_call_index = self.tool_call_index.saturating_add(1);
                i
            }
        }
    }

    /// Fields for a tool-call delta, emitted only the first time each is seen.
    /// A repeated id or name that *differs* from what was already sent is an
    /// upstream contradiction, not something to silently overwrite.
    fn tool_metadata_delta(
        &mut self,
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
    ) -> Result<Value, StreamFailure> {
        let seen = self.tool_metadata.entry(index).or_default();
        let mut delta = Map::new();

        for (field, value) in [("id", id), ("name", name)] {
            let Some(value) = value.filter(|v| !v.is_empty()) else {
                continue;
            };
            let slot = if field == "id" {
                &mut seen.id
            } else {
                &mut seen.name
            };
            match slot {
                Some(existing) if existing != value => {
                    return Err(StreamFailure::BadUpstreamData(
                        "Inconsistent upstream tool metadata".into(),
                    ));
                }
                Some(_) => {}
                None => {
                    *slot = Some(value.to_string());
                    delta.insert(field.into(), Value::String(value.to_string()));
                    // OpenAI's streaming type discriminator accompanies the id.
                    if field == "id" {
                        delta.insert("type".into(), Value::String("function".into()));
                    }
                }
            }
        }
        Ok(Value::Object(delta))
    }

    /// Translate one CC event into zero or more downstream records.
    ///
    /// An in-band `error` before any content is returned as `Err` so the caller
    /// can re-send upstream; after content it becomes a terminal error record.
    pub fn emit(&mut self, kind: &str, data: &Value) -> Result<Vec<Value>, StreamFailure> {
        // Terminal guard: everything after finish/error is swallowed, so a
        // misbehaving upstream cannot produce a second finish or post-terminal
        // content.
        if self.saw_finish {
            return Ok(Vec::new());
        }

        match kind {
            "start" => {
                self.tool_call_index = 0;
                Ok(vec![self.delta_chunk(json!({"role": "assistant"}))])
            }

            "text-delta" => {
                let Some(text) = text_of(data) else {
                    return Ok(Vec::new());
                };
                self.emitted_content = true;
                Ok(vec![self.delta_chunk(json!({"content": text}))])
            }

            "reasoning-delta" => {
                let Some(text) = text_of(data) else {
                    return Ok(Vec::new());
                };
                self.emitted_content = true;
                Ok(vec![self.delta_chunk(json!({"reasoning_content": text}))])
            }

            "tool-call-delta" => {
                self.emitted_content = true;
                let tool_call_id = data.get("toolCallId").and_then(Value::as_str);
                let upstream_index = data.get("index").and_then(Value::as_u64);
                let index = self.resolve_tool_call_index(tool_call_id, upstream_index);
                let args = data
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();

                let mut tc = Map::new();
                tc.insert("index".into(), json!(index));
                let metadata = self.tool_metadata_delta(
                    index,
                    tool_call_id,
                    data.get("name").and_then(Value::as_str),
                )?;
                // `id` and `type` sit on the tool-call object; `name` is part of
                // the function it names.
                let mut function = Map::new();
                if let Some(name) = metadata.get("name") {
                    function.insert("name".into(), name.clone());
                }
                function.insert("arguments".into(), Value::String(args.clone()));
                for key in ["id", "type"] {
                    if let Some(v) = metadata.get(key) {
                        tc.insert(key.into(), v.clone());
                    }
                }
                tc.insert("function".into(), Value::Object(function));

                let entry = self.tool_arguments.entry(index).or_default();
                entry.push_str(&args);

                Ok(vec![
                    self.delta_chunk(json!({"tool_calls": [Value::Object(tc)]}))
                ])
            }

            "tool-call" => {
                self.emitted_content = true;
                let tool_call_id = data
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let tool_name = data
                    .get("toolName")
                    .or_else(|| data.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = arguments_text(data);
                let index = self.resolve_tool_call_index(
                    Some(&tool_call_id)
                        .filter(|s| !s.is_empty())
                        .map(String::as_str),
                    data.get("index").and_then(Value::as_u64),
                );

                let emitted = self.tool_arguments.get(&index).cloned().unwrap_or_default();
                let suffix = tool_argument_suffix(&emitted, &args).map_err(|_| {
                    StreamFailure::BadUpstreamData("Inconsistent upstream tool arguments".into())
                })?;
                self.tool_arguments
                    .insert(index, format!("{emitted}{suffix}"));

                let metadata = self.tool_metadata_delta(
                    index,
                    Some(&tool_call_id)
                        .filter(|s| !s.is_empty())
                        .map(String::as_str),
                    Some(&tool_name)
                        .filter(|s| !s.is_empty())
                        .map(String::as_str),
                )?;
                // The final call carries the name when it has not been sent yet.
                let mut tc = Map::new();
                tc.insert("index".into(), json!(index));
                let mut function = Map::new();
                if let Some(name) = metadata.get("name").and_then(Value::as_str) {
                    function.insert("name".into(), Value::String(name.into()));
                }
                function.insert("arguments".into(), Value::String(suffix));
                tc.insert("function".into(), Value::Object(function));
                for key in ["id", "type"] {
                    if let Some(v) = metadata.get(key) {
                        tc.insert(key.into(), v.clone());
                    }
                }

                Ok(vec![
                    self.delta_chunk(json!({"tool_calls": [Value::Object(tc)]}))
                ])
            }

            "finish" => {
                self.saw_finish = true;
                let totals = extract_usage(data);
                self.last_usage = totals.clone();
                let reason = data
                    .get("finishReason")
                    .and_then(Value::as_str)
                    .unwrap_or("stop");
                let mut chunks = vec![self.envelope(json!([
                    {"index": 0, "delta": {}, "finish_reason": map_finish_reason(reason)}
                ]))];
                if let Some(totals) = totals {
                    chunks.push(self.usage_chunk(&totals));
                }
                Ok(chunks)
            }

            "error" => {
                let message = error_message(data);
                if !self.emitted_content {
                    // Recoverable: the caller re-sends and the client sees one
                    // clean response.
                    return Err(StreamFailure::UpstreamEvent(message));
                }
                self.saw_finish = true;
                crate::log::error(&format!("[CC upstream error] {message}"));
                Ok(vec![self.error_envelope(&message)])
            }

            _ => Ok(Vec::new()),
        }
    }

    fn usage_chunk(&self, totals: &UsageData) -> Value {
        let prompt = totals.prompt_tokens.unwrap_or(0);
        let completion = totals.completion_tokens.unwrap_or(0);
        let mut usage = Map::new();
        usage.insert("prompt_tokens".into(), json!(prompt));
        usage.insert("completion_tokens".into(), json!(completion));
        usage.insert(
            "total_tokens".into(),
            json!(totals
                .total_tokens
                .unwrap_or(prompt.saturating_add(completion))),
        );
        if let Some(cached) = totals.cached_tokens {
            usage.insert(
                "prompt_tokens_details".into(),
                json!({"cached_tokens": cached}),
            );
        }
        if let Some(reasoning) = totals.reasoning_tokens {
            usage.insert(
                "completion_tokens_details".into(),
                json!({"reasoning_tokens": reasoning}),
            );
        }
        let mut m = Map::new();
        m.insert("id".into(), Value::String(self.id.clone()));
        m.insert(
            "object".into(),
            Value::String("chat.completion.chunk".into()),
        );
        m.insert("created".into(), json!(self.created));
        m.insert("model".into(), Value::String(self.model.clone()));
        m.insert("choices".into(), json!([]));
        m.insert("usage".into(), Value::Object(usage));
        Value::Object(m)
    }

    /// The OpenAI error envelope. This shape — not a content chunk — is what
    /// signals failure: the client's chunk schema has an error arm that maps to
    /// a stream error part, which is what lets its caller retry. Wrapping the
    /// message in `delta.content` instead makes the client read the failure as
    /// an assistant reply and report a successful turn.
    ///
    /// `code: "network_error"` is the retry classifier's signal; `type` stays
    /// `upstream_error` for humans.
    pub fn error_envelope(&self, message: &str) -> Value {
        json!({
            "error": {
                "message": message,
                "type": "upstream_error",
                "code": "network_error",
            }
        })
    }

    /// Closing chunk for a stream that ended without a `finish` event, so the
    /// client does not see a truncated response.
    pub fn finish_chunks(&self, reason: &str) -> Vec<Value> {
        vec![self.envelope(json!([
            {"index": 0, "delta": {}, "finish_reason": reason}
        ]))]
    }

    /// Error record for a stream-level failure caught by the pump. Tags the
    /// message with its real origin so the cause stays visible.
    pub fn stream_error_chunks(&self, failure: &StreamFailure) -> Vec<Value> {
        crate::log::error(&format!("[CC upstream error] {}", failure.message()));
        vec![self.error_envelope(&failure.message())]
    }
}

/// Non-empty text payload, if any. An absent or empty `text` produces no
/// record at all, rather than an empty delta.
fn text_of(data: &Value) -> Option<String> {
    let text = data.get("text").and_then(Value::as_str).unwrap_or("");
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Canonical tool arguments as text: a string is used verbatim, any other value
/// is serialised, and a missing value becomes "".
fn arguments_text(data: &Value) -> String {
    let raw = data.get("input").or_else(|| data.get("arguments"));
    match raw {
        Some(Value::String(s)) => s.clone(),
        Some(v) if truthy(v) => serde_json::to_string(v).unwrap_or_default(),
        _ => String::new(),
    }
}

/// The human-readable message from an in-band error event.
fn error_message(data: &Value) -> String {
    if let Some(m) = data.get("message").and_then(Value::as_str) {
        return m.to_string();
    }
    if let Some(m) = data.pointer("/error/message").and_then(Value::as_str) {
        return m.to_string();
    }
    serde_json::to_string(data).unwrap_or_default()
}
