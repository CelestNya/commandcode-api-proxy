//! Reconcile canonical tool arguments with the bytes already streamed to the
//! client, ported from src/translate/tool-arguments.ts.
//!
//! CC commonly streams a tool call's arguments as deltas and then sends the
//! canonical object. Re-emitting the whole object would duplicate the payload;
//! emitting nothing would lose any part the deltas missed. This computes the
//! suffix that turns what was already sent into the canonical form, and fails
//! loudly when the two cannot be reconciled — which is a genuine upstream
//! contradiction, not something to paper over.

/// The upstream sent a canonical tool-call payload that contradicts the deltas
/// already forwarded. There is no recovery: the bytes on the wire cannot be
/// retracted, so the caller reports it as bad upstream data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InconsistentArguments;

/// The part of `canonical` not yet covered by `emitted`.
///
/// Returns `Ok("")` when the emitted bytes already are the canonical value
/// (possibly differing in insignificant whitespace), `Ok(suffix)` when a
/// suffix completes it, and `Err` when the two disagree.
pub fn tool_argument_suffix(
    emitted: &str,
    canonical: &str,
) -> Result<String, InconsistentArguments> {
    if let Some(rest) = canonical.strip_prefix(emitted) {
        return Ok(rest.to_string());
    }

    // Objects may be serialised with different whitespace or key order; if the
    // parsed values are equal there is nothing left to send.
    if json_values_equal(emitted, canonical) {
        return Ok(String::new());
    }

    // Byte-by-byte walk that ignores insignificant whitespace *outside* string
    // literals, so `{"a": 1}` and `{"a":1}` reconcile while `"a b"` does not
    // lose its space. Iterating over bytes is safe here because the skip set is
    // ASCII and a multi-byte character can never contain an ASCII byte.
    let emitted_bytes = emitted.as_bytes();
    let canonical_bytes = canonical.as_bytes();
    let mut offset = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for &byte in emitted_bytes {
        if !in_string {
            if is_json_whitespace(byte) {
                continue;
            }
            // Real content resumes: skip the canonical side's padding too.
            while canonical_bytes
                .get(offset)
                .copied()
                .is_some_and(is_json_whitespace)
            {
                offset = offset.saturating_add(1);
            }
        }
        if canonical_bytes.get(offset).copied() != Some(byte) {
            return Err(InconsistentArguments);
        }
        offset = offset.saturating_add(1);

        if escaped {
            escaped = false;
        } else if in_string && byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            in_string = !in_string;
        }
    }

    let suffix = canonical
        .get(offset..)
        .ok_or(InconsistentArguments)?
        .to_string();

    // Skipping whitespace must not splice two tokens into a new one: `1 2`
    // would otherwise be "completed" into `12`.
    if json_values_equal(&format!("{emitted}{suffix}"), canonical) {
        Ok(suffix)
    } else {
        Err(InconsistentArguments)
    }
}

fn is_json_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// Deep equality of two JSON texts. Unparseable input is never equal, which
/// mirrors `JSON.parse` throwing inside the original's try/catch.
fn json_values_equal(a: &str, b: &str) -> bool {
    match (
        serde_json::from_str::<serde_json::Value>(a),
        serde_json::from_str::<serde_json::Value>(b),
    ) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_prefix_leaves_the_remainder() {
        assert_eq!(
            tool_argument_suffix("{\"tz\":", "{\"tz\":\"UTC\"}"),
            Ok("\"UTC\"}".into())
        );
        assert_eq!(tool_argument_suffix("", "{}"), Ok("{}".into()));
        assert_eq!(tool_argument_suffix("{}", "{}"), Ok(String::new()));
    }

    #[test]
    fn whitespace_differences_reconcile_to_nothing() {
        // Same value, different formatting: nothing left to send.
        assert_eq!(
            tool_argument_suffix("{\"a\": 1}", "{\"a\":1}"),
            Ok(String::new())
        );
        assert_eq!(
            tool_argument_suffix("{\"a\":1}", "{\"a\": 1}"),
            Ok(String::new())
        );
        // Reordered keys are the same value too.
        assert_eq!(
            tool_argument_suffix("{\"a\":1,\"b\":2}", "{\"b\":2,\"a\":1}"),
            Ok(String::new())
        );
    }

    #[test]
    fn insignificant_whitespace_inside_the_streamed_prefix_is_skipped() {
        assert_eq!(
            tool_argument_suffix("{\"a\": ", "{\"a\":1}"),
            Ok("1}".into())
        );
    }

    #[test]
    fn whitespace_inside_a_string_is_significant() {
        // The space is content here, so the two disagree.
        assert_eq!(
            tool_argument_suffix("{\"a\":\"x y\"}", "{\"a\":\"xy\"}"),
            Err(InconsistentArguments)
        );
    }

    #[test]
    fn an_escaped_quote_does_not_close_the_string() {
        // `\"` inside the literal must not flip the in-string state, otherwise
        // the padding after it would be treated as insignificant.
        assert_eq!(
            tool_argument_suffix(r#"{"a":"x\""#, r#"{"a":"x\" y"}"#),
            Ok(" y\"}".into())
        );
    }
    #[test]
    fn splicing_two_tokens_apart_is_rejected() {
        // Dropping the space would forge the number 12 out of `1` and `2`.
        // The trailing space is what puts this on the whitespace-skipping path;
        // without it the canonical value is a plain prefix and "12" is a
        // legitimate continuation of "1".
        assert_eq!(tool_argument_suffix("1 ", "12"), Err(InconsistentArguments));
        assert_eq!(tool_argument_suffix("1", "12"), Ok("2".into()));
    }

    #[test]
    fn contradictions_are_rejected() {
        assert_eq!(
            tool_argument_suffix("{\"a\":1}", "{\"a\":2}"),
            Err(InconsistentArguments)
        );
        assert_eq!(
            tool_argument_suffix("{\"a\":1}", "{\"b\":1}"),
            Err(InconsistentArguments)
        );
        assert_eq!(
            tool_argument_suffix("garbage", "{\"a\":1}"),
            Err(InconsistentArguments)
        );
    }

    #[test]
    fn non_ascii_characters_align_by_bytes() {
        // Multi-byte characters can contain no ASCII byte, so byte iteration
        // cannot mistake their continuation bytes for structure.
        assert_eq!(
            tool_argument_suffix("{\"a\":\"日", "{\"a\":\"日本\"}"),
            Ok("本\"}".into())
        );
    }
}
