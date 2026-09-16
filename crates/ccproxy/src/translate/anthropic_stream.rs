//! CC events -> Anthropic Messages SSE records, ported from
//! `AnthropicStreamEncoder` in src/translate/anthropic.ts.
//!
//! The shape of this state machine is dictated by two client-visible facts:
//!
//! * Content blocks have lifetimes. A text or thinking block is closed as soon
//!   as a different block type opens, but tool_use blocks stay open until their
//!   arguments are complete, so interleaved tool deltas keep their indices.
//! * A thinking block must be closed with a `signature_delta` *inside* the
//!   block. Emitting it as a top-level event makes strict clients, which parse
//!   each record against a union keyed on `type`, abort the whole stream.
//!
//! The two terminal paths differ by exactly one record on purpose:
//! `terminal(None)` sends `message_delta(stop_reason="end_turn")`, which
//! clients treat as "the model finished normally", and `terminal(Some(f))`
//! deliberately does NOT. Appending the former after a failure turns a broken
//! turn into a successful one — the failure mode this project exists to avoid.

use crate::sse::{AnthropicRecord, StreamFailure};
use crate::tool_arguments::tool_argument_suffix;
use crate::translate::terminal::{
    canonical_arguments_text, error_message, map_stop_reason, THINKING_SIGNATURE,
};
use crate::translate::util::extract_usage;
use crate::usage::UsageData;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// `message_start` reports output_tokens as 1, not 0: the field is the count
/// "so far" and a 0 reads as a finished turn to some clients.
const INITIAL_OUTPUT_TOKENS: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq)]
enum BlockType {
    Text,
    Thinking,
    ToolUse,
}

#[derive(Debug, Clone)]
struct ToolBlock {
    index: usize,
    arguments: String,
    closed: bool,
}

pub struct AnthropicEncoder {
    message_id: String,
    model: String,
    block_index: usize,
    current_block_index: usize,
    current_block_type: Option<BlockType>,
    tool_blocks: HashMap<String, ToolBlock>,
    /// The upstream `start` event, held until a content event needs it: the
    /// token count on `message_start` comes from here.
    pending_start: Option<Value>,
    started: bool,
    pinged: bool,
    saw_finish: bool,
    /// Answer text or a tool call has gone out (`thinking` does not count).
    ///
    /// Distinct from `started`, which flips on the first content event of any
    /// kind. Replayed thinking is tolerable to a user (the block collapses);
    /// replayed answer text is not.
    saw_answer: bool,
    /// A replacement stream is being spliced onto what was already sent, so its
    /// replayed `start` and thinking are dropped.
    ///
    /// Measured in conformance/client-probes/retry-splice.mjs: re-sending
    /// `message_start` makes Anthropic clients drop the entire message.
    splice_replay: bool,
    pub last_usage: Option<UsageData>,
}

impl AnthropicEncoder {
    pub fn new(model: &str) -> Self {
        Self {
            message_id: format!("msg_{}", uuid::Uuid::new_v4()),
            model: model.to_string(),
            block_index: 0,
            current_block_index: 0,
            current_block_type: None,
            tool_blocks: HashMap::new(),
            pending_start: None,
            started: false,
            pinged: false,
            saw_finish: false,
            saw_answer: false,
            splice_replay: false,
            last_usage: None,
        }
    }

    /// The message id, reused by the non-streaming response builder.
    pub fn message_id(&self) -> &str {
        &self.message_id
    }

    pub fn finished(&self) -> bool {
        self.saw_finish
    }

    /// Whether the client has received anything worth keeping.
    ///
    /// `started` only flips when a content event is emitted; an upstream `start`
    /// is buffered and `message_start` is an empty envelope a replacement stream
    /// re-sends. So false means a failed attempt can be re-sent invisibly.
    pub fn has_emitted_content(&self) -> bool {
        self.started
    }

    /// Whether a failed attempt can be recovered by splicing a replacement
    /// stream onto the response the client is already reading.
    ///
    /// Allowed while no answer text and no tool call have gone out. A thinking
    /// block that already went out does not block it: the replayed thinking is
    /// dropped, so the client reads one continuous response. The 2026-09-15
    /// incident is exactly this case — CC errored after 7662 characters of
    /// reasoning with no answer, and the old `!started` gate refused to retry,
    /// losing the turn. 75 of 77 non-cancel in-stream failures rated on
    /// production logs land in this cell.
    pub fn can_splice_retry(&self) -> bool {
        !self.saw_answer
    }

    /// Enter splice mode: the next stream is a continuation, so its replayed
    /// `start` and thinking must be dropped rather than forwarded twice.
    pub fn begin_continuation(&mut self) {
        self.splice_replay = self.started;
    }

    /// Translate one CC event. An in-band `error` before any content is
    /// returned as `Err` so the caller can re-send upstream.
    pub fn emit(
        &mut self,
        kind: &str,
        data: &Value,
    ) -> Result<Vec<AnthropicRecord>, StreamFailure> {
        if self.saw_finish {
            return Ok(Vec::new());
        }

        match kind {
            "start" => {
                // A replayed `start` must not reset the block index: the
                // indices already handed out are part of what the client saw.
                if !self.splice_replay {
                    self.block_index = 0;
                    self.current_block_type = None;
                }
                self.pending_start = Some(data.clone());
                Ok(Vec::new())
            }

            "error" => {
                let message = error_message(data);
                crate::log::error(&format!("[CC upstream error] {message}"));
                if self.can_splice_retry() {
                    // Recoverable: the caller splices a replacement stream on
                    // and the client sees one continuous response. A thinking
                    // block already delivered does not block this — its replay
                    // is dropped while splicing.
                    return Err(StreamFailure::UpstreamEvent(message));
                }
                self.saw_finish = true;
                let mut records = Vec::new();
                self.close_current_block(&mut records);
                self.close_tool_blocks(&mut records);
                records.push(AnthropicRecord::new(
                    "error",
                    json!({
                        "error": {
                            "type": "overloaded_error",
                            "message": format!("[upstream-error] {message}"),
                        }
                    }),
                ));
                records.push(AnthropicRecord::new("message_stop", json!({})));
                Ok(records)
            }

            "finish" => Ok(self.handle_finish(data)),

            _ => {
                if !self.started {
                    let input_tokens = self
                        .pending_start
                        .as_ref()
                        .and_then(|d| d.pointer("/totalUsage/inputTokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let mut records = vec![self.make_message_start(input_tokens)];
                    self.started = true;
                    records.extend(self.handle_content(kind, data)?);
                    return Ok(records);
                }
                self.handle_content(kind, data)
            }
        }
    }

    fn handle_finish(&mut self, data: &Value) -> Vec<AnthropicRecord> {
        self.saw_finish = true;
        let mut records = Vec::new();

        // The client's parser requires message_start first, so a stream that
        // never produced content still gets a synthesised envelope rather than
        // one starting with message_delta.
        if !self.started {
            records.push(self.make_message_start(0));
            self.started = true;
        }

        self.close_current_block(&mut records);
        self.close_tool_blocks(&mut records);

        let reason = data
            .get("finishReason")
            .and_then(Value::as_str)
            .unwrap_or("stop");
        let usage = extract_usage(data);
        self.last_usage = usage.clone();

        // CC's `start` carries no usage, so message_start reported 0. This
        // message_delta *overwrites* the earlier usage rather than accumulating
        // it, which is what corrects the final counts. Absent fields are
        // omitted, never filled with 0: a fabricated cache_read_input_tokens
        // would read as a cache miss.
        let mut usage_json = Map::new();
        usage_json.insert(
            "input_tokens".into(),
            json!(usage.as_ref().and_then(|u| u.prompt_tokens).unwrap_or(0)),
        );
        usage_json.insert(
            "output_tokens".into(),
            json!(usage
                .as_ref()
                .and_then(|u| u.completion_tokens)
                .unwrap_or(0)),
        );
        if let Some(cached) = usage.as_ref().and_then(|u| u.cached_tokens) {
            usage_json.insert("cache_read_input_tokens".into(), json!(cached));
        }

        records.push(AnthropicRecord::new(
            "message_delta",
            json!({
                "delta": {"stop_reason": map_stop_reason(reason), "stop_sequence": null},
                "usage": Value::Object(usage_json),
            }),
        ));
        records.push(AnthropicRecord::new("message_stop", json!({})));
        records
    }

    fn handle_content(
        &mut self,
        kind: &str,
        data: &Value,
    ) -> Result<Vec<AnthropicRecord>, StreamFailure> {
        let mut records = Vec::new();

        match kind {
            "text-delta" => {
                if data
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.is_empty())
                {
                    self.saw_answer = true;
                }
                self.ensure_block_open(
                    &mut records,
                    BlockType::Text,
                    || json!({"type": "text", "text": ""}),
                );
                records.push(self.make_delta(
                    json!({"type": "text_delta", "text": data.get("text").and_then(Value::as_str).unwrap_or("")}),
                    None,
                ));
            }

            "reasoning-delta" => {
                // Replayed thinking during a splice is content the user has
                // already seen; sending it again would show the block twice.
                if self.splice_replay {
                    return Ok(records);
                }
                self.ensure_block_open(
                    &mut records,
                    BlockType::Thinking,
                    || json!({"type": "thinking", "thinking": ""}),
                );
                records.push(self.make_delta(
                    json!({"type": "thinking_delta", "thinking": data.get("text").and_then(Value::as_str).unwrap_or("")}),
                    None,
                ));
            }

            "tool-call-delta" => {
                self.saw_answer = true;
                let id = data.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                let name = data.get("name").and_then(Value::as_str).unwrap_or("");
                let index = self.ensure_tool_block(&mut records, id, name);
                let args = data
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let block = self.tool_blocks.get_mut(id).ok_or_else(|| {
                    StreamFailure::BadUpstreamData("Inconsistent upstream tool arguments".into())
                })?;
                if block.closed {
                    return Err(StreamFailure::BadUpstreamData(
                        "Inconsistent upstream tool arguments".into(),
                    ));
                }
                block.arguments.push_str(&args);
                records.push(self.make_delta(
                    json!({"type": "input_json_delta", "partial_json": args}),
                    Some(index),
                ));
            }

            "tool-call" => {
                self.saw_answer = true;
                let id = data.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                let name = data
                    .get("toolName")
                    .or_else(|| data.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let index = self.ensure_tool_block(&mut records, id, name);
                let args = canonical_arguments_text(data);
                let emitted = self
                    .tool_blocks
                    .get(id)
                    .map(|b| b.arguments.clone())
                    .unwrap_or_default();
                let suffix = tool_argument_suffix(&emitted, &args).map_err(|_| {
                    StreamFailure::BadUpstreamData("Inconsistent upstream tool arguments".into())
                })?;
                if !suffix.is_empty() {
                    let block = self.tool_blocks.get_mut(id).ok_or_else(|| {
                        StreamFailure::BadUpstreamData(
                            "Inconsistent upstream tool arguments".into(),
                        )
                    })?;
                    if block.closed {
                        return Err(StreamFailure::BadUpstreamData(
                            "Inconsistent upstream tool arguments".into(),
                        ));
                    }
                    block.arguments.push_str(&suffix);
                    // Built inline rather than via `make_delta`: that helper
                    // borrows `self`, which is already mutably borrowed here.
                    records.push(AnthropicRecord::new(
                        "content_block_delta",
                        json!({
                            "index": index,
                            "delta": {"type": "input_json_delta", "partial_json": suffix},
                        }),
                    ));
                }
                self.close_tool_block(&mut records, id);
            }

            _ => {}
        }

        Ok(records)
    }

    /// Open (or reuse) the block for a tool call id. Unlike text blocks, tool
    /// blocks are tracked per id and keep their index across interleaving.
    fn ensure_tool_block(
        &mut self,
        records: &mut Vec<AnthropicRecord>,
        id: &str,
        name: &str,
    ) -> usize {
        if let Some(existing) = self.tool_blocks.get(id) {
            return existing.index;
        }
        self.close_current_block(records);
        let index = self.block_index;
        self.block_index = self.block_index.saturating_add(1);
        self.current_block_type = Some(BlockType::ToolUse);
        self.current_block_index = index;
        records.push(AnthropicRecord::new(
            "content_block_start",
            json!({
                "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}},
            }),
        ));
        self.push_ping(records);
        self.tool_blocks.insert(
            id.to_string(),
            ToolBlock {
                index,
                arguments: String::new(),
                closed: false,
            },
        );
        // Tool blocks have independent lifetimes: a text block opening later
        // must not close this one, so it is not the "current" block any more.
        self.current_block_type = None;
        index
    }

    fn close_tool_block(&mut self, records: &mut Vec<AnthropicRecord>, id: &str) {
        let Some(block) = self.tool_blocks.get_mut(id) else {
            return;
        };
        if block.closed {
            return;
        }
        let index = block.index;
        block.closed = true;
        records.push(AnthropicRecord::new(
            "content_block_stop",
            json!({"index": index}),
        ));
    }

    fn close_tool_blocks(&mut self, records: &mut Vec<AnthropicRecord>) {
        let mut ids: Vec<String> = self.tool_blocks.keys().cloned().collect();
        // Hash order is not emission order; sort by index so blocks close in the
        // order they opened.
        ids.sort_by_key(|id| self.tool_blocks.get(id).map(|b| b.index).unwrap_or(0));
        for id in ids {
            self.close_tool_block(records, &id);
        }
    }

    fn ensure_block_open(
        &mut self,
        records: &mut Vec<AnthropicRecord>,
        block_type: BlockType,
        block: impl FnOnce() -> Value,
    ) {
        if self.current_block_type == Some(block_type) {
            return;
        }
        self.close_current_block(records);
        let index = self.block_index;
        self.block_index = self.block_index.saturating_add(1);
        self.current_block_type = Some(block_type);
        self.current_block_index = index;
        records.push(AnthropicRecord::new(
            "content_block_start",
            json!({"index": index, "content_block": block()}),
        ));
        self.push_ping(records);
    }

    /// Exactly one `ping` per stream, right after the first block opens. It
    /// exists to give slow first tokens something to arrive on.
    fn push_ping(&mut self, records: &mut Vec<AnthropicRecord>) {
        if self.pinged {
            return;
        }
        self.pinged = true;
        records.push(AnthropicRecord::new("ping", json!({})));
    }

    fn close_current_block(&mut self, records: &mut Vec<AnthropicRecord>) {
        let Some(block_type) = self.current_block_type else {
            return;
        };
        if block_type == BlockType::Thinking {
            // A delta type, not an event name: it belongs inside the block.
            records.push(self.make_delta(
                json!({"type": "signature_delta", "signature": THINKING_SIGNATURE}),
                None,
            ));
        }
        records.push(AnthropicRecord::new(
            "content_block_stop",
            json!({"index": self.current_block_index}),
        ));
        self.current_block_type = None;
    }

    fn make_delta(&self, delta: Value, index: Option<usize>) -> AnthropicRecord {
        let index = index.unwrap_or(self.current_block_index);
        AnthropicRecord::new(
            "content_block_delta",
            json!({"index": index, "delta": delta}),
        )
    }

    fn make_message_start(&self, input_tokens: u64) -> AnthropicRecord {
        AnthropicRecord::new(
            "message_start",
            json!({
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": INITIAL_OUTPUT_TOKENS,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0,
                        "service_tier": "standard",
                    },
                }
            }),
        )
    }

    /// Closing records for a stream that ended without a `finish` event, so the
    /// client sees a well-formed end-of-stream rather than a truncation.
    fn finish_records(&mut self, stop_reason: &str) -> Vec<AnthropicRecord> {
        let mut records = Vec::new();
        if !self.started {
            records.push(self.make_message_start(0));
            self.started = true;
        }
        self.close_current_block(&mut records);
        self.close_tool_blocks(&mut records);
        records.push(AnthropicRecord::new(
            "message_delta",
            json!({
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": {"input_tokens": 0, "output_tokens": 0},
            }),
        ));
        records.push(AnthropicRecord::new("message_stop", json!({})));
        records
    }

    /// Terminal records for a failure: report the error and stop, **without
    /// fabricating success**.
    ///
    /// The absent `message_delta` is the point. Clients read
    /// `message_delta(stop_reason="end_turn")` as "the model finished talking",
    /// so sending one after an error launders a mid-stream failure into a
    /// successful turn (measured in conformance/client-probes: finishReason
    /// became "stop"). `error` followed directly by `message_stop` leaves the
    /// client's finishReason non-stop.
    fn error_records(&mut self, failure: &StreamFailure) -> Vec<AnthropicRecord> {
        self.saw_finish = true;
        let mut records = Vec::new();
        // message_start must be the first record, so an unstarted stream gets
        // an empty envelope before the error.
        if !self.started {
            records.push(self.make_message_start(0));
            self.started = true;
        }
        // An open block that never closes makes the client think the stream was
        // cut off mid-block.
        self.close_current_block(&mut records);
        self.close_tool_blocks(&mut records);
        records.push(AnthropicRecord::new(
            "error",
            json!({
                "error": {"type": "overloaded_error", "message": failure.message()},
            }),
        ));
        records.push(AnthropicRecord::new("message_stop", json!({})));
        records
    }

    /// The closing records for a stream that reached its end — cleanly with
    /// `failure == None`, or with the failure the client is being told about.
    ///
    /// The protocol this owns: an `error` record is terminal. No
    /// `message_delta` may follow it, and a finished stream must not be
    /// terminated twice — both would fabricate a success the upstream never
    /// delivered. See `error_records` for why the delta is omitted.
    pub fn terminal(&mut self, failure: Option<&StreamFailure>) -> Vec<AnthropicRecord> {
        if self.finished() {
            return Vec::new();
        }
        match failure {
            Some(f) => self.error_records(f),
            None => self.finish_records("end_turn"),
        }
    }
}
