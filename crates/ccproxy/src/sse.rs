//! SSE framing and mid-stream failure taxonomy, ported from src/stream.ts.
//!
//! Two hard constraints live here. SSE records are terminated by a blank line —
//! a stream that ends without one has its last pending event discarded by the
//! client. And an Anthropic record's `event:` name must equal its payload's
//! `type`: the two SDKs discriminate on opposite fields, so a record that
//! satisfies only one of them aborts the whole stream for that client.
//! `AnthropicRecord::new` is the single constructor that enforces the second.

use serde_json::{Map, Value};

/// OpenAI-style record: `data: <json>\n\n`.
pub fn format_sse(data: &Value) -> String {
    format!("data: {}\n\n", compact(data))
}

/// The bare `[DONE]` sentinel. Emitted exactly once, at the end of an OpenAI
/// stream, and never inside the JSON envelope.
pub fn format_sse_done() -> String {
    "data: [DONE]\n\n".to_string()
}

/// Serialise without whitespace, the way `JSON.stringify` does.
fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// One Anthropic SSE record. `event` and the payload's `type` are kept equal by
/// construction, so no call site can desynchronise them.
#[derive(Debug, Clone, PartialEq)]
pub struct AnthropicRecord {
    pub event: &'static str,
    pub data: Value,
}

impl AnthropicRecord {
    /// Build a record. The payload's `type` is set to the event name rather
    /// than assumed: a record whose `type` is missing or disagrees is the
    /// failure mode this type exists to prevent.
    pub fn new(event: &'static str, data: Value) -> Self {
        let data = match data {
            Value::Object(mut m) => {
                m.insert("type".into(), Value::String(event.into()));
                Value::Object(m)
            }
            other => {
                // Non-object payloads cannot carry a discriminator; wrap them
                // so the invariant still holds on the wire.
                let mut m = Map::new();
                m.insert("type".into(), Value::String(event.into()));
                m.insert("value".into(), other);
                Value::Object(m)
            }
        };
        Self { event, data }
    }

    pub fn to_sse(&self) -> String {
        format!("event: {}\ndata: {}\n\n", self.event, compact(&self.data))
    }
}

/// A mid-stream failure, classified by origin.
///
/// The Node version classifies by inspecting `err.name`, `err.code` and the
/// message text, in that order. Encoding the classes as a type removes the
/// string matching while keeping the same precedence and the same tag for each
/// class, since the downstream error *type* is what its classifier reads — the
/// tag exists to keep the real cause legible to a human.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamFailure {
    /// The upstream produced nothing at all within the no-output window.
    ///
    /// Distinct from [`Self::IdleTimeout`]: no byte has reached the client yet,
    /// so the attempt can be discarded and re-sent from scratch without the
    /// client ever knowing — the retry-from-zero path.
    NoOutput { ms: u64 },
    /// Upstream socket stayed open but stopped producing bytes.
    IdleTimeout { ms: u64 },
    /// The upstream reported failure in-band, before anything was delivered.
    UpstreamEvent(String),
    /// Transport failed mid-body; the wording matches what the Node transport
    /// reports for this class, so the client sees the same message.
    ConnectionReset,
    /// The upstream's own events contradicted each other (e.g. a tool-call's
    /// canonical arguments disagreeing with the deltas already sent).
    BadUpstreamData(String),
    /// The downstream client went away.
    ClientGone,
    /// Anything else; reported verbatim.
    Other(String),
}

impl StreamFailure {
    /// The bracket prefix, per class.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::NoOutput { .. } => "[no-output]",
            Self::IdleTimeout { .. } => "[idle-timeout]",
            Self::UpstreamEvent(_) => "[upstream-error]",
            Self::ConnectionReset => "[connection-reset]",
            Self::BadUpstreamData(_) => "[bad-upstream-data]",
            Self::ClientGone => "[client-gone]",
            Self::Other(_) => "[stream-error]",
        }
    }

    /// The message body, without the tag.
    pub fn detail(&self) -> String {
        match self {
            Self::NoOutput { ms } => format!("CC upstream produced no output for {ms}ms"),
            Self::IdleTimeout { ms } => format!("CC upstream idle timeout: no data for {ms}ms"),
            Self::UpstreamEvent(m) | Self::BadUpstreamData(m) | Self::Other(m) => m.clone(),
            Self::ConnectionReset => "terminated".to_string(),
            Self::ClientGone => "client disconnected".to_string(),
        }
    }

    /// Tagged message, e.g. `[idle-timeout] CC upstream idle timeout: no data for 800ms`.
    pub fn tagged(&self) -> String {
        format!("{} {}", self.tag(), self.detail())
    }

    /// The error message the downstream client receives.
    pub fn message(&self) -> String {
        self.tagged()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_are_blank_line_terminated() {
        assert_eq!(format_sse(&json!({"a": 1})), "data: {\"a\":1}\n\n");
        assert_eq!(format_sse_done(), "data: [DONE]\n\n");
    }

    #[test]
    fn anthropic_records_always_carry_their_discriminator() {
        let r = AnthropicRecord::new("message_stop", json!({}));
        assert_eq!(
            r.to_sse(),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
    }

    #[test]
    fn a_disagreeing_type_is_corrected_to_the_event_name() {
        // The invariant is structural: no call site can emit the two fields
        // disagreeing, which is what aborts a strict client's stream parse.
        let r = AnthropicRecord::new("content_block_stop", json!({"type": "wrong", "index": 0}));
        assert_eq!(r.data["type"], "content_block_stop");
        assert_eq!(r.data["index"], json!(0));
    }

    #[test]
    fn failure_tags_match_the_documented_classes() {
        assert_eq!(
            StreamFailure::IdleTimeout { ms: 800 }.tagged(),
            "[idle-timeout] CC upstream idle timeout: no data for 800ms"
        );
        assert_eq!(
            StreamFailure::UpstreamEvent("model refused".into()).tagged(),
            "[upstream-error] model refused"
        );
        assert_eq!(
            StreamFailure::ConnectionReset.tagged(),
            "[connection-reset] terminated"
        );
        assert_eq!(
            StreamFailure::BadUpstreamData("Inconsistent upstream tool arguments".into()).tagged(),
            "[bad-upstream-data] Inconsistent upstream tool arguments"
        );
        assert_eq!(
            StreamFailure::ClientGone.tagged(),
            "[client-gone] client disconnected"
        );
        assert_eq!(
            StreamFailure::Other("boom".into()).tagged(),
            "[stream-error] boom"
        );
    }
}
