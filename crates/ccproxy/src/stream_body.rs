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
use serde_json::Value;
use std::io::Read;
use std::sync::{Arc, Mutex};

/// Which downstream dialect is being written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dialect {
    Openai,
    Anthropic,
}

/// The dialect-specific half of an `SseBody`, resolved once instead of at every
/// call site. The two encoders implement the same lifecycle (splice gates,
/// emit, terminal, usage) and differ only in the wire shape they produce, so
/// the dispatch lives here: adding a third dialect touches this enum and the
/// two `match` arms, not eight call sites.
enum Encoder<'a> {
    Openai(&'a mut OpenAIEncoder),
    Anthropic(&'a mut AnthropicEncoder),
}

impl Encoder<'_> {
    /// Whether anything worth keeping has reached the client yet.
    fn has_emitted_content(&self) -> bool {
        match self {
            Self::Openai(e) => e.has_emitted_content(),
            Self::Anthropic(e) => e.has_emitted_content(),
        }
    }

    /// Whether a failed attempt can be recovered by splicing a replacement
    /// stream onto the response already in flight.
    fn can_splice_retry(&self) -> bool {
        match self {
            Self::Openai(e) => e.can_splice_retry(),
            Self::Anthropic(e) => e.can_splice_retry(),
        }
    }

    /// Enter splice mode: the replacement's replayed opening is dropped.
    fn begin_continuation(&mut self) {
        match self {
            Self::Openai(e) => e.begin_continuation(),
            Self::Anthropic(e) => e.begin_continuation(),
        }
    }

    /// Encode one CC event into its SSE bytes for this dialect.
    fn emit_sse(&mut self, kind: &str, data: &Value) -> Result<Vec<u8>, StreamFailure> {
        let mut out = Vec::new();
        match self {
            Self::Openai(e) => {
                for chunk in e.emit(kind, data)? {
                    out.extend_from_slice(format_sse(&chunk).as_bytes());
                }
            }
            Self::Anthropic(e) => {
                for record in e.emit(kind, data)? {
                    out.extend_from_slice(record.to_sse().as_bytes());
                }
            }
        }
        Ok(out)
    }

    /// The terminal records for this dialect, SSE-formatted. The OpenAI
    /// terminal protocol ends with the bare `[DONE]` sentinel.
    fn terminal_sse(&mut self, failure: Option<&StreamFailure>) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Openai(e) => {
                for chunk in e.terminal(failure) {
                    out.extend_from_slice(format_sse(&chunk).as_bytes());
                }
                out.extend_from_slice(format_sse_done().as_bytes());
            }
            Self::Anthropic(e) => {
                for record in e.terminal(failure) {
                    out.extend_from_slice(record.to_sse().as_bytes());
                }
            }
        }
        out
    }

    /// The usage the encoder has observed so far, if any.
    fn last_usage(&self) -> Option<UsageData> {
        match self {
            Self::Openai(e) => e.last_usage.clone(),
            Self::Anthropic(e) => e.last_usage.clone(),
        }
    }
}

/// A slot the body fills with the usage it observed, so the caller can record
/// it after the response has been written. The body itself is consumed by
/// `respond`, so the totals cannot be read off the encoders afterwards.
pub type UsageSlot = Arc<Mutex<Option<UsageData>>>;

/// Where a streaming attempt's outcome is reported, as it happens.
///
/// A stream is not one attempt: a failure part-way through may be followed by a
/// splice onto a replacement, so the attempt that died must be recorded *then*,
/// while the replacement's own outcome comes later. That timing is why this is
/// a callback rather than a return value — by the time the body is consumed, the
/// per-attempt detail is gone.
pub trait StreamOutcomeSink: Send + Sync {
    /// An attempt died after delivering only thinking, and a replacement is
    /// being spliced on. Its usage is unknowable: CC reports usage only in the
    /// terminal `finish` event, so a broken stream has no numbers to give.
    fn interrupted(&self, tag: &str);
    /// The client went away, so the attempt was cut short deliberately. This is
    /// not CC's fault and must not be recorded as an upstream error.
    fn aborted(&self, tag: &str);
    /// An attempt failed for good; the client is being told.
    fn failed(&self, tag: &str, usage: Option<&UsageData>);
    /// The first byte is being handed downstream (time to first byte).
    fn ttfb(&self);
}

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
    /// Tracks whether the first byte has been reported yet.
    ttfb_reported: bool,
    /// Re-sends the request for a replacement attempt. `None` disables the
    /// recovery path (used where re-sending would be wrong, e.g. tests).
    reconnect: Option<Box<dyn FnMut() -> Result<UpstreamStream, StreamFailure> + Send>>,
    /// Where the observed usage is published once the stream is done.
    usage: UsageSlot,
    /// Where each attempt's outcome is reported. `None` when unaccounted.
    outcome: Option<Arc<dyn StreamOutcomeSink>>,
}

impl SseBody {
    pub fn new(
        upstream: UpstreamStream,
        dialect: Dialect,
        model: &str,
        reconnect: Option<Box<dyn FnMut() -> Result<UpstreamStream, StreamFailure> + Send>>,
        usage: UsageSlot,
    ) -> Self {
        Self::with_outcome(upstream, dialect, model, reconnect, usage, None)
    }

    /// As `new`, but reporting each attempt's outcome to `outcome`.
    pub fn with_outcome(
        upstream: UpstreamStream,
        dialect: Dialect,
        model: &str,
        reconnect: Option<Box<dyn FnMut() -> Result<UpstreamStream, StreamFailure> + Send>>,
        usage: UsageSlot,
        outcome: Option<Arc<dyn StreamOutcomeSink>>,
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
            ttfb_reported: false,
            reconnect,
            usage,
            outcome,
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

    /// The dialect half of this body, resolved once per call instead of at
    /// every use site.
    fn encoder(&mut self) -> Encoder<'_> {
        match self.dialect {
            Dialect::Openai => Encoder::Openai(&mut self.openai),
            Dialect::Anthropic => Encoder::Anthropic(&mut self.anthropic),
        }
    }

    /// Whether anything worth keeping has reached the client yet. Decides
    /// whether a failed attempt may be re-sent invisibly.
    pub fn has_emitted_content(&mut self) -> bool {
        self.encoder().has_emitted_content()
    }

    /// Whether a failed attempt can be recovered by splicing a replacement
    /// stream onto the response already in flight.
    ///
    /// Wider than [`Self::has_emitted_content`]: reasoning already delivered
    /// does not block recovery, because the replacement's replayed reasoning is
    /// dropped. Answer text does block it, because the user would read the same
    /// paragraph twice.
    fn can_splice_retry(&mut self) -> bool {
        self.encoder().can_splice_retry()
    }

    fn begin_continuation(&mut self) {
        self.encoder().begin_continuation();
    }

    fn encode(&mut self, event: crate::ndjson::CCEvent) -> Result<(), StreamFailure> {
        let bytes = self.encoder().emit_sse(&event.kind, &event.data)?;
        self.out.extend_from_slice(&bytes);
        Ok(())
    }

    /// Queue the closing records for a stream that ended (cleanly or not).
    fn terminal(&mut self, failure: Option<StreamFailure>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.publish_usage();
        // Report a failure that ends the turn for good. A failure that is about
        // to be spliced is reported by `try_replacement` instead, so this only
        // sees the attempt the client is actually told about.
        let observed = self.last_usage();
        if let (Some(sink), Some(f)) = (self.outcome.as_ref(), failure.as_ref()) {
            match f {
                // The client left, so this attempt was cut short on purpose:
                // recording it as an upstream error would blame CC for our own
                // abort.
                StreamFailure::ClientGone => sink.aborted(f.tag()),
                _ => sink.failed(f.tag(), observed.as_ref()),
            }
        }
        // The encoder owns the terminal protocol: an error envelope is
        // terminal, a finish chunk after it would claim a normal stop.
        let bytes = self.encoder().terminal_sse(failure.as_ref());
        self.out.extend_from_slice(&bytes);
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
                        if self.try_replacement(&f)? {
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
                    if self.try_replacement(&f)? {
                        continue;
                    }
                    self.terminal(Some(f));
                    return Ok(());
                }
            }
        }
    }

    /// The usage the encoders have observed so far, if any.
    fn last_usage(&mut self) -> Option<UsageData> {
        self.encoder().last_usage()
    }

    /// Re-send upstream once, splicing the replacement onto the same response.
    /// Returns whether it was replaced.
    ///
    /// Safe while nothing worth keeping has been delivered. Bytes already handed
    /// to the writer cannot be taken back, so the replacement must continue the
    /// stream rather than restart it — hence `begin_continuation`, which drops
    /// the replayed opening and reasoning.
    ///
    /// `can_splice_retry` is the whole gate: this runs only from `produce()`,
    /// which `read()` calls only with the buffer drained (`out_pos == 0`), so
    /// undelivered bytes cannot leak out and delivered ones are covered by
    /// `saw_answer`.
    fn try_replacement(&mut self, failure: &StreamFailure) -> Result<bool, StreamFailure> {
        if self.retried || !self.can_splice_retry() {
            return Ok(false);
        }
        let Some(reconnect) = self.reconnect.as_mut() else {
            return Ok(false);
        };
        self.retried = true;
        crate::log::warn("[stream] continuing after upstream failure");
        let replaced = reconnect()?;
        // The attempt that just died is recorded before the replacement is read:
        // its usage is unknowable (CC reports usage only at the end), so this
        // becomes a NULL row rather than a fabricated zero.
        if let Some(sink) = self.outcome.as_ref() {
            sink.interrupted(failure.tag());
        }
        self.upstream = replaced;
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
        let observed = self.last_usage();
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
        // The first bytes are about to be handed over, so this is the moment
        // the client actually waited for.
        if !self.ttfb_reported {
            self.ttfb_reported = true;
            if let Some(sink) = self.outcome.as_ref() {
                sink.ttfb();
            }
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
    let built = collector.response(dialect, model, id);
    // An in-band error must not read as an assistant reply.
    built.map_err(|_| StreamFailure::Other("CC upstream generation failed".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build the dispatch handle exactly the way `SseBody` does.
    fn encoder<'a>(
        dialect: Dialect,
        openai: &'a mut OpenAIEncoder,
        anthropic: &'a mut AnthropicEncoder,
    ) -> Encoder<'a> {
        match dialect {
            Dialect::Openai => Encoder::Openai(openai),
            Dialect::Anthropic => Encoder::Anthropic(anthropic),
        }
    }

    /// The dialect dispatch is one handle, not a match at every call site: each
    /// lifecycle step must be reachable through the same enum for both
    /// dialects, and the splice gates must behave identically (they are the
    /// recovery path's only safety).
    #[test]
    fn the_dispatch_handle_serves_the_lifecycle_for_both_dialects() {
        for dialect in [Dialect::Openai, Dialect::Anthropic] {
            let mut openai = OpenAIEncoder::new("deepseek-v4-flash");
            let mut anthropic = AnthropicEncoder::new("deepseek-v4-flash");
            let mut enc = encoder(dialect, &mut openai, &mut anthropic);

            assert!(!enc.has_emitted_content(), "{dialect:?} starts dirty");
            assert!(
                enc.can_splice_retry(),
                "{dialect:?} blocks splice before anything"
            );

            let _ = enc.emit_sse("start", &json!({"type": "message"})).unwrap();
            let delta = enc.emit_sse("text-delta", &json!({"text": "hi"})).unwrap();
            assert!(!delta.is_empty(), "{dialect:?} dropped a text delta");
            assert!(enc.has_emitted_content(), "{dialect:?} ignores content");
            assert!(
                !enc.can_splice_retry(),
                "{dialect:?} still allows splice after answer text"
            );

            let terminal = enc.terminal_sse(None);
            assert!(!terminal.is_empty(), "{dialect:?} terminal is empty");
            assert!(
                String::from_utf8_lossy(&terminal).contains("data: "),
                "{dialect:?} terminal is not SSE"
            );
        }
    }

    /// The splice path (the golden `error-after-reasoning-recovered` shape): a
    /// failure after reasoning but before answer text may re-send invisibly,
    /// and `begin_continuation` must keep the replacement's replayed thinking
    /// out of the client's hands.
    #[test]
    fn the_dispatch_handle_supports_the_reasoning_splice() {
        for dialect in [Dialect::Openai, Dialect::Anthropic] {
            let mut openai = OpenAIEncoder::new("deepseek-v4-flash");
            let mut anthropic = AnthropicEncoder::new("deepseek-v4-flash");
            let mut enc = encoder(dialect, &mut openai, &mut anthropic);

            let _ = enc
                .emit_sse("reasoning-delta", &json!({"text": "think"}))
                .unwrap();
            // Reasoning alone must not block recovery — this is the 2026-09-15
            // incident cell.
            assert!(
                enc.can_splice_retry(),
                "{dialect:?} blocks splice on reasoning"
            );
            enc.begin_continuation();
            // The replayed thinking is dropped; the replacement's first answer
            // text still flows, exactly once.
            let replay = enc
                .emit_sse("reasoning-delta", &json!({"text": "replayed"}))
                .unwrap();
            assert!(
                replay.is_empty(),
                "{dialect:?} replayed thinking during a splice"
            );
            let delta = enc.emit_sse("text-delta", &json!({"text": "hi"})).unwrap();
            assert!(
                !delta.is_empty(),
                "{dialect:?} lost answer text after a splice"
            );
            assert!(
                !enc.can_splice_retry(),
                "{dialect:?} allows a second splice"
            );
        }
    }
}
