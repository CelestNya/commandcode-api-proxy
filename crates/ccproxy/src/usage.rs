//! Usage accounting, ported from src/usage-stats.ts. M1 keeps the in-process
//! totals that `/health` exposes; durable persistence moves to sqlite in M7.

use serde::Serialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Default)]
pub struct UsageTotals {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CacheSnapshot {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub cache_rate: f64,
}

/// Percentage with one decimal, 0 when there is no prompt volume.
fn rate(cached: u64, prompt: u64) -> f64 {
    if prompt == 0 {
        return 0.0;
    }
    let raw = (cached as f64 / prompt as f64) * 1000.0;
    (raw.round()) / 10.0
}

impl UsageTotals {
    pub fn snapshot(&self) -> CacheSnapshot {
        CacheSnapshot {
            requests: self.requests,
            prompt_tokens: self.prompt_tokens,
            cached_tokens: self.cached_tokens,
            completion_tokens: self.completion_tokens,
            cache_rate: rate(self.cached_tokens, self.prompt_tokens),
        }
    }
}

impl CacheSnapshot {
    /// The wire shape `/health` exposes (camelCase, matching the Node output).
    pub fn to_json(&self) -> Value {
        json!({
            "requests": self.requests,
            "promptTokens": self.prompt_tokens,
            "cachedTokens": self.cached_tokens,
            "completionTokens": self.completion_tokens,
            "cacheRate": self.cache_rate,
        })
    }
}

/// Token estimate for `count_tokens`: CJK codepoints count 1, everything else
/// 4 characters per token, rounded up — mirrors the Node implementation.
// Counters over a string already in memory: overflow needs a 2^64-char body,
// which the request-size guard rejects long before this runs.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "char counters bounded by the request body size limit"
)]
pub fn estimate_tokens(body: &Value) -> u64 {
    let mut parts: Vec<String> = Vec::new();
    match body.get("system") {
        Some(Value::String(s)) => parts.push(s.clone()),
        Some(Value::Array(blocks)) => {
            for b in blocks {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        parts.push(t.to_string());
                    }
                }
            }
        }
        _ => {}
    }
    if let Some(msgs) = body.get("messages").and_then(Value::as_array) {
        for msg in msgs {
            match msg.get("content") {
                Some(Value::String(s)) => parts.push(s.clone()),
                Some(other) => parts.push(other.to_string()),
                None => {}
            }
        }
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for t in tools {
            parts.push(
                t.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            );
            parts.push(
                t.get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            );
            let schema = t.get("input_schema").cloned().unwrap_or_else(|| json!({}));
            parts.push(schema.to_string());
        }
    }

    let all: String = parts.concat();
    let mut cjk: u64 = 0;
    let mut non_cjk: u64 = 0;
    for ch in all.chars() {
        let code = ch as u32;
        let is_cjk = (0x4e00..=0x9fff).contains(&code)
            || (0x3040..=0x309f).contains(&code)
            || (0x30a0..=0x30ff).contains(&code)
            || (0xac00..=0xd7af).contains(&code);
        if is_cjk {
            cjk += 1;
        } else {
            non_cjk += 1;
        }
    }
    cjk + non_cjk.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_totals_report_zero_rate() {
        let s = UsageTotals::default().snapshot();
        assert_eq!(s.cache_rate, 0.0);
        assert_eq!(s.requests, 0);
    }

    #[test]
    fn rate_is_one_decimal() {
        let t = UsageTotals {
            requests: 4,
            prompt_tokens: 400,
            cached_tokens: 256,
            completion_tokens: 20,
        };
        let s = t.snapshot();
        assert_eq!(s.cache_rate, 64.0);
        assert_eq!(s.to_json()["requests"], 4);
        assert_eq!(s.to_json()["promptTokens"], 400);
    }

    #[test]
    fn estimate_counts_cjk_per_char() {
        let body = json!({"messages": [{"content": "你好"}]});
        assert_eq!(estimate_tokens(&body), 2);
    }

    #[test]
    fn estimate_quarters_ascii_and_rounds_up() {
        let body = json!({"messages": [{"content": "abcdefgh"}]});
        assert_eq!(estimate_tokens(&body), 2);
        let body = json!({"messages": [{"content": "abcde"}]});
        assert_eq!(estimate_tokens(&body), 2);
    }
}
