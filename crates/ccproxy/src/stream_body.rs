//! Streaming the encoded records downstream, and collapsing a stream into a
//! non-streaming response. Ported from `pumpStream` and `collectEvents` in
//! src/server.ts.
//!
//! The SSE body is exposed as a `Read` rather than written eagerly: tiny_http
//! pulls from it as the socket accepts bytes, so a slow client applies
//! backpressure all the way to the upstream socket instead of the proxy
//! buffering the whole answer. Dropping the reader (which is what happens when
//! the client disconnects) drops the upstream connection with it, which is what
//! stops CC generating tokens nobody will read.
//!
//! A failure that happens *after* the 200 has been sent cannot become an HTTP
//! status anymore, so it is reported in-band, in the dialect's own error shape,
//! followed by the dialect's terminal record. The one thing it must never do is
//! look like a clean end of stream: that would turn a truncated answer into a
//! successful turn.

use crate::sse::{format_sse, format_sse_done, StreamFailure};
use crate::translate::{AnthropicEncoder, Collected, NonStreamingCollector, OpenAIEncoder};
use crate::upstream::UpstreamStream;
use crate::usage::UsageData;
use std::io::Read;
use std::sync::{Arc, Mutex};

/// Which downstream dialect is being written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dialect {
    Openai,
    Anthropic,
}

/// A slot the body fills with the usage it observed, so the caller can record
/// it after the response has been written. The body itself is consumed by
/// `respond`, so the totals cannot be read off the encoders afterwards.
pub type UsageSlot = Arc<Mutex<Option<UsageData>>>;

/// The SSE byte stream for one response.
pub struct SseBody {
    upstream: UpstreamStream,
    dialect: Dialect,
    openai: OpenAIEncoder,
    anthropic: AnthropicEncoder,
    /// Bytes produced but not yet handed to the writer.
    out: Vec<u8>,
    out_pos: usize,
    /// Set once the terminal records are queued; `read` then reports EOF.
    finished: bool,
    /// One replacement attempt is allowed while nothing has been delivered.
    retried: bool,
    /// Re-sends the request for a replacement attempt. `None` disables the
    /// recovery path (used where re-sending would be wrong, e.g. tests).
    reconnect: Option<Box<dyn FnMut() -> Result<UpstreamStream, StreamFailure> + Send>>,
    /// Where the observed usage is published once the stream is done.
    usage: UsageSlot,
}

impl SseBody {
    pub fn new(
        upstream: UpstreamStream,
        dialect: Dialect,
        model: &str,
        reconnect: Option<Box<dyn FnMut() -> Result<UpstreamStream, StreamFailure> + Send>>,
        usage: UsageSlot,
    ) -> Self {
        Self {
            upstream,
            dialect,
            openai: OpenAIEncoder::new(model),
            anthropic: AnthropicEncoder::new(model),
            out: Vec::new(),
            out_pos: 0,
            finished: false,
            retried: false,
            reconnect,
            usage,
        }
    }

    /// An unused slot, for tests and for callers that do not account usage.
    pub fn new_slot() -> UsageSlot {
        Arc::new(Mutex::new(None))
    }

    /// The OpenAI encoder, for usage accounting and the non-streaming id.
    pub fn openai_encoder(&self) -> &OpenAIEncoder {
        &self.openai
    }

    /// The Anthropic encoder, for usage accounting and the non-streaming id.
    pub fn anthropic_encoder(&self) -> &AnthropicEncoder {
        &self.anthropic
    }

    /// Whether anything worth keeping has reached the client yet. Decides
    /// whether a failed attempt may be re-sent invisibly.
    pub fn has_emitted_content(&self) -> bool {
        match self.dialect {
            Dialect::Openai => self.openai.has_emitted_content(),
            Dialect::Anthropic => self.anthropic.has_emitted_content(),
        }
    }

    /// Whether a failed attempt can be recovered by splicing a replacement
    /// stream onto the response already in flight.
    ///
    /// Wider than [`Self::has_emitted_content`]: reasoning already delivered
    /// does not block recovery, because the replacement's replayed reasoning is
    /// dropped. Answer text does block it, because the user would read the same
    /// paragraph twice.
    fn can_splice_retry(&self) -> bool {
        match self.dialect {
            Dialect::Openai => self.openai.can_splice_retry(),
            Dialect::Anthropic => self.anthropic.can_splice_retry(),
        }
    }

    fn begin_continuation(&mut self) {
        match self.dialect {
            Dialect::Openai => self.openai.begin_continuation(),
            Dialect::Anthropic => self.anthropic.begin_continuation(),
        }
    }

    fn encode(&mut self, event: crate::ndjson::CCEvent) -> Result<(), StreamFailure> {
        match self.dialect {
            Dialect::Openai => {
                for chunk in self.openai.emit(&event.kind, &event.data)? {
                    self.out.extend_from_slice(format_sse(&chunk).as_bytes());
                }
            }
            Dialect::Anthropic => {
                for record in self.anthropic.emit(&event.kind, &event.data)? {
                    self.out.extend_from_slice(record.to_sse().as_bytes());
                }
            }
        }
        Ok(())
    }

    /// Queue the closing records for a stream that ended (cleanly or not).
    fn terminal(&mut self, failure: Option<StreamFailure>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.publish_usage();
        match (self.dialect, failure) {
            (Dialect::Openai, Some(f)) => {
                // The error envelope and nothing else: a finish chunk after it
                // would claim a normal stop, which is what makes a client
                // record a failed turn as successful.
                for chunk in self.openai.stream_error_chunks(&f) {
                    self.out.extend_from_slice(format_sse(&chunk).as_bytes());
                }
                self.out.extend_from_slice(format_sse_done().as_bytes());
            }
            (Dialect::Openai, None) => {
                if !self.openai.finished() {
                    for chunk in self.openai.finish_chunks("stop") {
                        self.out.extend_from_slice(format_sse(&chunk).as_bytes());
                    }
                }
                self.out.extend_from_slice(format_sse_done().as_bytes());
            }
            (Dialect::Anthropic, Some(f)) => {
                // `error_records` emits error + message_stop without a trailing
                // message_delta, which would overwrite the failure with
                // stop_reason "end_turn".
                if !self.anthropic.finished() {
                    for record in self.anthropic.error_records(&f, true) {
                        self.out.extend_from_slice(record.to_sse().as_bytes());
                    }
                }
            }
            (Dialect::Anthropic, None) => {
                if !self.anthropic.finished() {
                    for record in self.anthropic.finish_records("end_turn") {
                        self.out.extend_from_slice(record.to_sse().as_bytes());
                    }
                }
            }
        }
    }

    /// Advance until there are bytes to hand downstream, or the stream is over.
    fn produce(&mut self) -> Result<(), StreamFailure> {
        loop {
            match self.upstream.next_event() {
                Ok(Some(event)) => match self.encode(event) {
                    Ok(()) => {
                        if self.out_pos < self.out.len() {
                            return Ok(());
                        }
                        // An encoder may legitimately produce nothing (a ping,
                        // or a record after the terminal one), so keep pulling.
                    }
                    Err(f) => {
                        if self.try_replacement()? {
                            continue;
                        }
                        self.terminal(Some(f));
                        return Ok(());
                    }
                },
                Ok(None) => {
                    self.terminal(None);
                    return Ok(());
                }
                Err(f) => {
                    if self.try_replacement()? {
                        continue;
                    }
                    self.terminal(Some(f));
                    return Ok(());
                }
            }
        }
    }

    /// Re-send upstream once, splicing the replacement onto the same response.
    /// Returns whether it was replaced.
    ///
    /// Safe while nothing worth keeping has been delivered. Bytes already handed
    /// to the writer cannot be taken back, so the replacement must continue the
    /// stream rather than restart it — hence `begin_continuation`, which drops
    /// the replayed opening and reasoning.
    fn try_replacement(&mut self) -> Result<bool, StreamFailure> {
        if self.retried || self.out_pos > 0 || !self.can_splice_retry() {
            return Ok(false);
        }
        let Some(reconnect) = self.reconnect.as_mut() else {
            return Ok(false);
        };
        self.retried = true;
        crate::log::warn("[stream] continuing after upstream failure");
        self.upstream = reconnect()?;
        self.begin_continuation();
        Ok(true)
    }

    /// Copy out the next slice of produced bytes.
    // The arithmetic is index bookkeeping over a buffer this struct owns:
    // `out_pos` never exceeds `out.len()` (it only advances by `take`, which is
    // itself clamped to the remaining length), so the subtract cannot wrap and
    // the add cannot overflow a real allocation.
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "bounded index arithmetic over an owned buffer; see comment"
    )]
    fn take_out(&mut self, buf: &mut [u8]) -> usize {
        let take = (self.out.len() - self.out_pos).min(buf.len());
        if let (Some(src), Some(dst)) = (
            self.out.get(self.out_pos..self.out_pos + take),
            buf.get_mut(..take),
        ) {
            dst.copy_from_slice(src);
            self.out_pos += take;
        }
        take
    }

    /// Hand the observed usage to the caller. Published at terminal time, which
    /// is when the `finish` event (the only carrier of usage) has been seen.
    fn publish_usage(&mut self) {
        let observed = match self.dialect {
            Dialect::Openai => self.openai.last_usage.clone(),
            Dialect::Anthropic => self.anthropic.last_usage.clone(),
        };
        if let Ok(mut slot) = self.usage.lock() {
            *slot = observed;
        }
    }
}

impl Read for SseBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Hand over anything already produced.
        if self.out_pos < self.out.len() {
            return Ok(self.take_out(buf));
        }
        // The terminal records were the last bytes; a repeated call is EOF.
        if self.finished {
            return Ok(0);
        }

        self.out.clear();
        self.out_pos = 0;
        self.produce()
            .map_err(|f| std::io::Error::other(f.tagged()))?;
        if self.out.is_empty() {
            return Ok(0);
        }
        Ok(self.take_out(buf))
    }
}

/// Drain a whole stream, collapsing it into the non-streaming response for
/// `dialect`.
///
/// The upstream is always read as a stream, so a non-streaming request is
/// collapsed here. Draining fully also means a failure to terminate surfaces
/// before the status line is chosen, which is what lets a failed generation
/// become a 502 instead of a 200 with an empty body.
pub fn collect_non_streaming(
    upstream: &mut UpstreamStream,
    dialect: Dialect,
    model: &str,
    id: &str,
) -> Result<Collected, StreamFailure> {
    let mut collector = NonStreamingCollector::new();
    upstream.pump(|event| {
        collector.push(&event.kind, &event.data);
        // Never stop early: the whole stream is needed to build the response.
        Ok(())
    })?;
    let built = match dialect {
        Dialect::Openai => collector.openai_response(model, id),
        Dialect::Anthropic => collector.anthropic_response(model, id),
    };
    // An in-band error must not read as an assistant reply.
    built.map_err(|_| StreamFailure::Other("CC upstream generation failed".into()))
}
