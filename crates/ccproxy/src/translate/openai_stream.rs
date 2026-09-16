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
    /// Answer text or a tool call has gone out (`reasoning` does not count).
    ///
    /// Distinct from `emitted_content`, which flips on the first reasoning
    /// delta: replayed reasoning is nearly invisible to a user, replayed answer
    /// text is not. The two gate different decisions.
    saw_answer: bool,
    /// A replacement stream is being spliced onto what was already sent, so its
    /// replayed opening (role chunk) and reasoning are dropped.
    ///
    /// Measured in conformance/client-probes/retry-splice.mjs: a client that
    /// receives the answer twice is worse off than one that gets an honest
    /// error.
    splice_replay: bool,
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
            saw_answer: false,
            splice_replay: false,
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

    /// Whether a failed attempt can be recovered by splicing a replacement
    /// stream onto the response the client is already reading.
    ///
    /// Allowed while no answer text and no tool call have gone out. Reasoning
    /// already delivered does not block it: the replayed reasoning is dropped,
    /// and a user barely notices a thinking block restarting. Answer text does
    /// block it, because the user would read the same paragraph twice.
    pub fn can_splice_retry(&self) -> bool {
        !self.saw_answer
    }

    /// Enter splice mode: the next stream is a continuation, so its replayed
    /// opening and reasoning are duplicates and must be dropped.
    ///
    /// Only suppresses replay when something was already sent — with nothing
    /// sent, that opening *is* what the client sees first.
    pub fn begin_continuation(&mut self) {
        self.splice_replay = self.emitted_content;
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
                // A replayed `start` re-numbers tool calls from zero, which
                // would collide with indices already handed out, and repeats a
                // role chunk the client has merged. With nothing sent yet it is
                // the opening of the response and must go out.
                if self.splice_replay {
                    return Ok(Vec::new());
                }
                self.tool_call_index = 0;
                Ok(vec![self.delta_chunk(json!({"role": "assistant"}))])
            }

            "text-delta" => {
                let Some(text) = text_of(data) else {
                    return Ok(Vec::new());
                };
                self.emitted_content = true;
                self.saw_answer = true;
                Ok(vec![self.delta_chunk(json!({"content": text}))])
            }

            "reasoning-delta" => {
                // Replayed reasoning during a splice is content the user has
                // already seen.
                if self.splice_replay {
                    return Ok(Vec::new());
                }
                let Some(text) = text_of(data) else {
                    return Ok(Vec::new());
                };
                self.emitted_content = true;
                Ok(vec![self.delta_chunk(json!({"reasoning_content": text}))])
            }

            "tool-call-delta" => {
                self.emitted_content = true;
                self.saw_answer = true;
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
                self.saw_answer = true;
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
                if self.can_splice_retry() {
                    // Recoverable: the caller splices a replacement stream on
                    // and the client sees one continuous response. The role
                    // chunk alone does not count as output (clients merge a
                    // repeated one), and neither does reasoning, which is
                    // dropped while splicing.
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
    fn error_envelope(&self, message: &str) -> Value {
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
    fn finish_chunks(&self, reason: &str) -> Vec<Value> {
        vec![self.envelope(json!([
            {"index": 0, "delta": {}, "finish_reason": reason}
        ]))]
    }

    /// Error record for a stream-level failure caught by the pump. Tags the
    /// message with its real origin so the cause stays visible.
    fn stream_error_chunks(&self, failure: &StreamFailure) -> Vec<Value> {
        crate::log::error(&format!("[CC upstream error] {}", failure.message()));
        vec![self.error_envelope(&failure.message())]
    }

    /// The closing chunks for a stream that reached its end — cleanly with
    /// `failure == None`, or with the failure the client is being told about.
    ///
    /// The protocol this owns: an error envelope is terminal. A finish chunk
    /// after it would claim a normal stop, which is what makes a client record
    /// a failed turn as successful, so a failed stream gets the envelope and
    /// nothing else. The `[DONE]` sentinel is the SSE framing layer's job and
    /// is deliberately not added here.
    pub fn terminal(&mut self, failure: Option<&StreamFailure>) -> Vec<Value> {
        match failure {
            Some(f) => {
                self.saw_finish = true;
                self.stream_error_chunks(f)
            }
            None => {
                if self.finished() {
                    Vec::new()
                } else {
                    self.finish_chunks("stop")
                }
            }
        }
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
