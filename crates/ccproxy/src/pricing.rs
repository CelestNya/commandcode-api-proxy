//! Model price table, maintained by crawling Command Code's public pricing page.
//!
//! Command Code bills the Provider API at the underlying model rates, and its
//! pricing page changes: new models appear, DEAL badges apply temporary
//! multipliers, free models come and go while capacity lasts. A static table
//! in the repo would go stale, so this module fetches the page, parses the
//! per-model rows and caches the result next to the billing database.
//!
//! Refresh policy: on load, if the cached table is missing or older than 24
//! hours, a synchronous fetch replaces it. A failed fetch (offline, page
//! restructured) keeps the previous cache — the proxy must keep working and
//! the ledger keeps its numbers; only the cost columns go missing.
//!
//! Cost model: a request bills cached prompt tokens at the cache-read rate,
//! the remaining prompt at the input rate and completion at the output rate.
//! All rates are USD per million tokens.

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the live prices live. Public, no auth, changes over time — exactly
/// why we crawl instead of vendoring the numbers.
pub const PRICING_SOURCE: &str = "https://commandcode.ai/docs/resources/pricing-limits";
/// Re-fetch when the cached copy is older than this.
const REFRESH_AFTER_SECS: u64 = 24 * 3600;
/// Network budget for one fetch; on timeout the old cache stays.
const FETCH_TIMEOUT: Duration = Duration::from_secs(12);
const USER_AGENT: &str = concat!(
    "ccproxy-pricing/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/CelestNya/commandcode-api-proxy)"
);

/// One rate triple in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RateSet {
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
}

/// USD per million tokens for one model.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    /// Most open models expose no cache-write rate; `None` means the column
    /// showed "—" on the source page.
    pub cache_write: Option<f64>,
    /// API model id as recorded in the ledger, when the page's embedded data
    /// carries it (e.g. "deepseek-v4.1-flash").
    pub id: Option<String>,
    /// Time-of-day rates; only a few models have peak/off-peak pricing.
    pub peak: Option<RateSet>,
    pub off_peak: Option<RateSet>,
}

impl ModelPrice {
    /// Rates that apply at `utc_minute_of_day` (minutes since UTC midnight).
    ///
    /// Command Code's peak windows are 01:00–04:00 and 06:00–10:00 UTC
    /// (7 hours a day, full price); the other 17 hours are off-peak and
    /// roughly half price. Models without time-of-day data are flat.
    pub fn rate_at(&self, utc_minute_of_day: u32) -> RateSet {
        match (self.peak, self.off_peak) {
            (Some(pk), Some(op)) => {
                let in_peak = (60..240).contains(&utc_minute_of_day)
                    || (360..600).contains(&utc_minute_of_day);
                if in_peak {
                    pk
                } else {
                    op
                }
            }
            _ => RateSet {
                input: self.input,
                output: self.output,
                cache_read: self.cache_read,
            },
        }
    }
}

/// Minutes since UTC midnight — the input for peak/off-peak selection.
///
/// This is "now"; for a ledger row use [`minute_of_day_from_iso8601`] instead,
/// so the row is priced at the hour it actually ran.
pub fn utc_minute_of_day() -> u32 {
    ((crate::now_epoch_secs() % 86_400) / 60) as u32
}

/// Minute-of-day of a ledger `ts` (`2026-09-20T17:52:17.917Z`, already UTC).
///
/// Returns `None` for a shape it does not recognise, so a caller can fall back
/// to the current time rather than mis-price at minute 0.
// Bounds are checked immediately above the arithmetic (hh <= 23, mm <= 59), so
// the product is at most 1380 and the sum at most 1439.
#[allow(clippy::arithmetic_side_effects)]
pub fn minute_of_day_from_iso8601(ts: &str) -> Option<u32> {
    // "YYYY-MM-DDTHH:MM:SS…" — offset 11 is the hour, 14 the minute.
    let bytes = ts.as_bytes();
    if bytes.len() < 16 || bytes.get(10) != Some(&b'T') {
        return None;
    }
    let hh: u32 = ts.get(11..13)?.parse().ok()?;
    let mm: u32 = ts.get(14..16)?.parse().ok()?;
    if hh > 23 || mm > 59 {
        return None;
    }
    Some(hh * 60 + mm)
}

/// The full table plus provenance, persisted as `pricing.json`.
#[derive(Debug, Clone)]
pub struct Pricing {
    pub models: BTreeMap<String, ModelPrice>,
    /// Epoch seconds of the last successful fetch; `None` when no cache
    /// exists yet.
    pub fetched_at: Option<u64>,
    pub source: &'static str,
}

impl Pricing {
    /// An empty table for "nothing known yet"; the UI renders no cost columns.
    pub fn empty() -> Self {
        Pricing {
            models: BTreeMap::new(),
            fetched_at: None,
            source: PRICING_SOURCE,
        }
    }

    /// Load the table for `dir`, refreshing the cache when it is missing or
    /// stale. Never panics and never blocks the caller for long: the fetch is
    /// capped at `FETCH_TIMEOUT` and a failure returns the previous cache.
    pub fn load(dir: &Path) -> Pricing {
        let mut cached = read_cache(dir).unwrap_or_else(Pricing::empty);
        if cached.stale() {
            if let Some(fresh) = fetch_latest() {
                cached = fresh;
            }
            write_cache(dir, &cached);
        }
        cached
    }

    /// Rate lookup for an exact model id as recorded in the ledger.
    pub fn price(&self, model: &str) -> Option<&ModelPrice> {
        if let Some(p) = self.models.get(model) {
            return Some(p);
        }
        // The embedded page data gives us the true API id; a ledger row may
        // use it directly.
        if let Some(p) = self
            .models
            .values()
            .find(|m| m.id.as_deref() == Some(model))
        {
            return Some(p);
        }
        // Normalize both sides ("DeepSeek V4.1 Flash" → "deepseek-v4.1-flash").
        // Exact equality only, so "gpt-5" can never match "gpt-5-pro".
        let want = normalize(model);
        if let Some(p) = self.find_normalized(&want) {
            return Some(p);
        }
        // Ledger names carry a provider prefix ("deepseek/deepseek-v4.1-flash");
        // retry with the bare id.
        if let Some((_, id)) = model.rsplit_once('/') {
            let want = normalize(id);
            if let Some(p) = self.find_normalized(&want) {
                return Some(p);
            }
            if let Some(p) = self.find_by_key(&match_key(id)) {
                return Some(p);
            }
        }
        // Last resort: drop the separators entirely, so a vendor space on the
        // page ("Qwen 3.8 Flash") still meets a client's spelling
        // ("qwen3.8-flash"). Still exact on the alphanumerics.
        self.find_by_key(&match_key(model))
    }

    fn find_normalized(&self, want: &str) -> Option<&ModelPrice> {
        self.models
            .iter()
            .find(|(k, _)| normalize(k) == *want)
            .map(|(_, v)| v)
    }

    fn find_by_key(&self, want: &str) -> Option<&ModelPrice> {
        self.models
            .iter()
            .find(|(k, _)| match_key(k) == *want)
            .map(|(_, v)| v)
    }

    /// Cost in USD of one ledger row: cache-write for tokens that created a
    /// cache entry, cache-read for tokens that hit it, input for the remaining
    /// uncached prompt, output for completion tokens.
    ///
    /// `utc_minute_of_day` is the minute-of-day **of the request this row
    /// describes**, not of the moment the total is computed: a peak-hour row
    /// keeps its peak price when viewed later. Callers reading ledger rows must
    /// derive it from the row's own `ts` (see [`minute_of_day_from_iso8601`]);
    /// passing the current time would silently restate history at today's rate.
    pub fn cost(
        &self,
        model: &str,
        prompt: i64,
        cached: i64,
        cache_creation: i64,
        completion: i64,
        utc_minute_of_day: u32,
    ) -> Option<f64> {
        let m = self.price(model)?;
        let r = m.rate_at(utc_minute_of_day);
        // promptTokens includes both cachedTokens and cacheCreationTokens, so
        // each is a subset of the prompt; only the rest is truly uncached.
        let creation_n = cache_creation.max(0) as f64;
        let cached_n = cached.max(0) as f64;
        let uncached = prompt
            .saturating_sub(cached)
            .saturating_sub(cache_creation)
            .max(0) as f64;
        let completion_n = completion.max(0) as f64;
        // Most open models expose no cache-write rate; bill those tokens at
        // the input rate so a missing column never understates the cost.
        let write_rate = m.cache_write.unwrap_or(r.input);
        Some(
            (r.input * uncached
                + r.cache_read * cached_n
                + write_rate * creation_n
                + r.output * completion_n)
                / 1e6,
        )
    }

    fn stale(&self) -> bool {
        match self.fetched_at {
            None => true,
            Some(ts) => {
                let aged = crate::now_epoch_secs().saturating_sub(ts) > REFRESH_AFTER_SECS;
                // Cache files written before the embedded-payload upgrade carry
                // no ids at all; refresh once so peak/off-peak rates appear.
                aged || self.models.values().all(|m| m.id.is_none())
            }
        }
    }
}

/// Fetch and parse the current table from the official page.
pub fn fetch_latest() -> Option<Pricing> {
    let resp = ureq::get(PRICING_SOURCE)
        .set("User-Agent", USER_AGENT)
        .timeout(FETCH_TIMEOUT)
        .call()
        .ok()?;
    if resp.status() != 200 {
        return None;
    }
    let mut body = String::new();
    resp.into_reader().read_to_string(&mut body).ok()?;
    // The page embeds a structured model list (Next.js RSC payload) with the
    // true API ids and peak/off-peak rates; prefer it. Fall back to the HTML
    // grid when the payload moves, and always patch cache-write from the grid
    // (the payload does not carry it).
    let mut models = parse_models_json(&body);
    if models.is_empty() {
        models = parse_pricing_html(&body);
    } else {
        patch_cache_write(&mut models, &body);
    }
    if models.is_empty() {
        return None; // the page no longer parses — keep whatever we had
    }
    Some(Pricing {
        models,
        fetched_at: Some(crate::now_epoch_secs()),
        source: PRICING_SOURCE,
    })
}

/// Parse the embedded model list out of the raw page.
///
/// The docs page is a Next.js app; the rendered payload contains a chunk of
/// the form `"models":[{"id":"deepseek-v4.1-flash","name":"DeepSeek V4.1
/// Flash","inputCost":0.15,"outputCost":0.6,"cacheReadCost":0.003,
/// "timeOfDay":{"peak":{...},"offPeak":{...}}},…]`. Quotes are JS-escaped
/// (`\"`) in the HTML source. This path yields the API ids and the
/// peak/off-peak rates the table rows do not show.
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
fn parse_models_json(body: &str) -> BTreeMap<String, ModelPrice> {
    let mut out = BTreeMap::new();
    const NEEDLE: &str = r#"\"models\":["#;
    let Some(start) = body.find(NEEDLE) else {
        return out;
    };
    // The needle ends with the opening '['; walk it to the matching ']',
    // skipping escaped quotes so a `\"` inside a string is not a bracket.
    let mut i = start;
    let mut depth = 0usize;
    let bytes = body.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
            i += 2;
            continue;
        }
        match bytes[i] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if depth != 0 {
        return out;
    }
    // The needle ends with the opening bracket; the JSON array starts there.
    let raw = &body[start + NEEDLE.len() - 1..=i];
    // Undo the JS-string escaping of quotes; \uXXXX stays intact for serde.
    let unescaped = raw.replace(r#"\""#, "\"");
    let Ok(v) = serde_json::from_str::<Value>(&unescaped) else {
        return out;
    };
    let Some(arr) = v.as_array() else {
        return out;
    };
    for m in arr {
        let (Some(name), Some(input), Some(output), Some(cache_read)) = (
            m.get("name").and_then(|x| x.as_str()),
            m.get("inputCost").and_then(|x| x.as_f64()),
            m.get("outputCost").and_then(|x| x.as_f64()),
            m.get("cacheReadCost").and_then(|x| x.as_f64()),
        ) else {
            continue;
        };
        let tod = m.get("timeOfDay");
        let peak = tod.and_then(|x| x.get("peak")).and_then(rate_from_json);
        let off_peak = tod.and_then(|x| x.get("offPeak")).and_then(rate_from_json);
        out.insert(
            name.to_string(),
            ModelPrice {
                input,
                output,
                cache_read,
                cache_write: None,
                id: m.get("id").and_then(|x| x.as_str()).map(String::from),
                peak,
                off_peak,
            },
        );
    }
    out
}

fn rate_from_json(v: &Value) -> Option<RateSet> {
    // The embedded payload names the fields inputCost/outputCost/cacheReadCost;
    // the cache file stores input/output/cacheRead. Accept both.
    let num = |a: &str, b: &str, c: &str| {
        v.get(a)
            .and_then(|x| x.as_f64())
            .or_else(|| v.get(b).and_then(|x| x.as_f64()))
            .or_else(|| v.get(c).and_then(|x| x.as_f64()))
    };
    Some(RateSet {
        input: num("input", "inputCost", "input")?,
        output: num("output", "outputCost", "output")?,
        // cache files written before the rename used the raw field name
        cache_read: num("cacheRead", "cacheReadCost", "cache_read")?,
    })
}

/// The embedded list omits cache-write rates; the grid carries them. Merge by
/// display name, only filling fields the payload left empty.
fn patch_cache_write(models: &mut BTreeMap<String, ModelPrice>, body: &str) {
    let grid = parse_pricing_html(body);
    for (name, g) in &grid {
        if let Some(e) = models.get_mut(name) {
            if e.cache_write.is_none() {
                e.cache_write = g.cache_write;
            }
        }
    }
}

/// Parse the pricing table out of the raw page.
///
/// The table is a Tailwind grid; every row is
/// `<div role="row">…</div>` with the model name first and then five
/// `px-2 py-3` cells: context window, input/M, output/M, cache-read/M and
/// cache-write/M (the last one is often "—"). DEAL rows strike the old price
/// and show the discounted one in a `font-semibold` span, free models put
/// "Free" in the price cells. Parsing is deliberately format-driven and
/// strict: any row that does not yield an input and output rate is skipped,
/// so a restructured page degrades to fewer rows instead of wrong numbers.
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
pub fn parse_pricing_html(html: &str) -> BTreeMap<String, ModelPrice> {
    let mut out = BTreeMap::new();
    let mut rest = html;
    while let Some(p) = rest.find("role=\"row\"") {
        let row_start = p + "role=\"row\"".len();
        let body = &rest[row_start..];
        let row_end = body.find("role=\"row\"").unwrap_or(body.len());
        if let Some((name, price)) = parse_row(&body[..row_end]) {
            out.insert(name, price);
        }
        rest = &body[row_end..];
    }
    out
}

/// One `<div role="row">` body → `(model id, price)`, or `None` when the row
/// is the header or has been restructured beyond recognition.
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
fn parse_row(row: &str) -> Option<(String, ModelPrice)> {
    let marker = "<span class=\"truncate text-[13px] font-medium";
    let mi = row.find(marker)?;
    let after = &row[mi + marker.len()..];
    let name = name_text(after)?;
    if name.is_empty() || name == "Model" {
        return None;
    }
    // cells[0] is the context-window cell (text-[11px]); the four price
    // cells follow. Table headers also carry px-2 py-3 cells but no prices.
    let cells = div_cells(row);
    if cells.len() < 5 {
        return None;
    }
    let input = cell_price(cells[1])?;
    let output = cell_price(cells[2])?;
    let cache_read = cell_price(cells[3])?;
    let cache_write = cell_price(cells[4]);
    Some((
        name,
        ModelPrice {
            input,
            output,
            cache_read,
            cache_write,
            id: None,
            peak: None,
            off_peak: None,
        },
    ))
}

/// Text of the model-name span, tolerating an inner `<a>` (DEAL rows wrap
/// the name in a link).
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
fn name_text(after: &str) -> Option<String> {
    let open_end = after.find('>')?;
    let mut name = &after[open_end + 1..];
    if name.starts_with("<a") {
        let link_end = name.find('>')?;
        name = &name[link_end + 1..];
    }
    let close = name.find('<')?;
    let text = name[..close].trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Split the row into its `px-2 py-3` / `px-3 py-3` cells, in document order
/// (the name cell is `px-4` and is intentionally not captured). Deal rows
/// wrap price columns in `flex flex-col items-end px-2 py-3`, so the match
/// is on the class *containing* the padding token, not on a fixed prefix.
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
fn div_cells(row: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut rest = row;
    while let Some(p) = next_cell(rest) {
        let open_end = rest[p..].find('>').map(|j| p + j + 1).unwrap_or(rest.len());
        rest = &rest[open_end..];
        let end = next_cell(rest).unwrap_or(rest.len());
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
    out
}

/// Byte offset (relative to `rest`) of the next `px-2/px-3 py-3` cell opening
/// div. The scan advances internally, so the returned offset is the base
/// offset plus the in-slice position — callers index `rest` with it.
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
fn next_cell(rest: &str) -> Option<usize> {
    let mut scan = rest;
    let mut base = 0;
    loop {
        let p = scan.find("<div class=\"")?;
        let val_start = scan[p..].find('"').map(|j| p + j + 1)?;
        let val_end = scan[val_start..].find('"').map(|j| val_start + j)?;
        let class_attr = &scan[val_start..val_end];
        if class_attr.contains("px-2 py-3") || class_attr.contains("px-3 py-3") {
            return Some(base + p);
        }
        let next = val_end + 1;
        base += next;
        scan = &scan[next..];
    }
}

/// Numeric value of one price cell, in USD per million tokens.
///
/// `Free` → 0; a plain `$0.15` → 0.15; a DEAL cell shows the struck old
/// price first and the discounted one second, so the last `$` amount wins;
/// "—" or any non-price text → `None`.
/// Lowercase the name and fold separators so page names and API ids meet:
/// "DeepSeek V4.1 Flash" -> "deepseek-v4.1-flash".
#[allow(clippy::arithmetic_side_effects)]
fn normalize(name: &str) -> String {
    // Page names carry editorial suffixes ("DeepSeek V4 Flash (latest)") that
    // have no API-id counterpart; drop parenthesised segments first, then
    // fold every remaining separator to '-'.
    let mut out = String::with_capacity(name.len());
    let mut depth = 0usize;
    for c in name.trim().chars() {
        match c {
            // The segment's own leading separator ("V4 Flash (latest)")
            // belongs to the suffix, not to the id: drop it with the parens.
            '(' => {
                if out.ends_with([' ', '-']) {
                    out.pop();
                }
                depth += 1;
            }
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    // '.' folds with the other separators: the page writes "Qwen 3.8 Flash"
    // while a client asks for "qwen3.8-flash", and the ledger stores the
    // client's spelling.
    out.to_ascii_lowercase()
        .replace([' ', '/', '\\', '_', '.'], "-")
}

/// The comparison key for a model name: [`normalize`] with every separator
/// removed.
///
/// The page writes "Qwen 3.8 Flash"; a client asks for "qwen3.8-flash" (no
/// space after the vendor). Those normalize to `qwen-3-8-flash` and
/// `qwen3-8-flash` — equal only once separators are dropped. Comparing on this
/// key as a fallback is deliberately looser than [`normalize`], but still
/// exact on the alphanumeric content, so "gpt-5" and "gpt5" match while
/// "gpt-5" and "gpt-5-pro" do not.
fn match_key(name: &str) -> String {
    normalize(name).chars().filter(|c| *c != '-').collect()
}
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
fn cell_price(cell: &str) -> Option<f64> {
    if cell.contains("uppercase") && cell.contains("Free") {
        return Some(0.0);
    }
    let mut last: Option<f64> = None;
    let mut rest = cell;
    while let Some(d) = rest.find('$') {
        let after = &rest[d + 1..];
        let num_end = after
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(after.len());
        if let Ok(v) = after[..num_end].parse::<f64>() {
            last = Some(v);
        }
        rest = after;
    }
    last
}

// ── cache ───────────────────────────────────────────────────────

fn cache_path(dir: &Path) -> PathBuf {
    dir.join("pricing.json")
}

fn read_cache(dir: &Path) -> Option<Pricing> {
    let text = fs::read_to_string(cache_path(dir)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let fetched_at = v.get("fetchedAt").and_then(|x| x.as_u64());
    let models = v.get("models")?.as_object()?;
    let mut map = BTreeMap::new();
    for (k, mv) in models {
        let input = mv.get("input")?.as_f64()?;
        let output = mv.get("output")?.as_f64()?;
        let cache_read = mv.get("cacheRead")?.as_f64()?;
        let cache_write = mv.get("cacheWrite").and_then(|x| x.as_f64());
        let id = mv.get("id").and_then(|x| x.as_str()).map(String::from);
        let peak = mv.get("peak").and_then(rate_from_json);
        let off_peak = mv.get("offPeak").and_then(rate_from_json);
        map.insert(
            k.clone(),
            ModelPrice {
                input,
                output,
                cache_read,
                cache_write,
                id,
                peak,
                off_peak,
            },
        );
    }
    Some(Pricing {
        models: map,
        fetched_at,
        source: PRICING_SOURCE,
    })
}

fn write_cache(dir: &Path, p: &Pricing) {
    let mut models = serde_json::Map::new();
    for (k, m) in &p.models {
        models.insert(
            k.clone(),
            json!({
                "input": m.input,
                "output": m.output,
                "cacheRead": m.cache_read,
                "cacheWrite": m.cache_write,
                "id": m.id,
                "peak": m.peak,
                "offPeak": m.off_peak,
            }),
        );
    }
    let v = json!({
        "fetchedAt": p.fetched_at,
        "source": p.source,
        "models": models,
    });
    if let Ok(text) = serde_json::to_string(&v) {
        let _ = fs::write(cache_path(dir), text);
    }
}

// ── tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A faithful miniature of the Tailwind grid rows, covering a plain row,
    /// a DEAL row (struck price + discounted), a free row and the header.
    const SAMPLE: &str = r#"
<div role="row">
<div class="flex min-w-0 items-center gap-2 px-4 py-3"><span class="truncate text-[13px] font-medium normal-case tracking-normal text-foreground">DeepSeek V4.1 Flash</span></div>
<div class="px-2 py-3 text-right text-[11px] text-muted-foreground tabular-nums">1M</div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums"><span class="underline decoration-dotted"><span>$0.15</span></span></div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums"><span><span>$0.60</span></span></div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums"><span><span>$0.003</span></span></div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums"><span class="text-muted-foreground">—</span></div>
<div class="px-3 py-3"><button type="button" aria-label="Capabilities: Text input, Reasoning"></button></div>
</div>
<div role="row">
<div class="flex min-w-0 items-center gap-2 px-4 py-3"><span class="truncate text-[13px] font-medium normal-case tracking-normal text-foreground underline"><a data-state="closed">MiniMax M3</a></span><a aria-label="View MiniMax M3 deal details" class="shrink-0 border border-border bg-foreground px-1.5 py-0.5 text-[10px] font-semibold">-50%</a></div>
<div class="px-2 py-3 text-right text-[11px] text-muted-foreground tabular-nums">1M</div>
<div class="flex flex-col items-end px-2 py-3 text-[12px] leading-tight tabular-nums"><s class="text-[11px] text-muted-foreground">$0.60</s><span class="font-semibold text-foreground">$0.30</span></div>
<div class="flex flex-col items-end px-2 py-3 text-[12px] leading-tight tabular-nums"><s class="text-[11px] text-muted-foreground">$2.40</s><span class="font-semibold text-foreground">$1.20</span></div>
<div class="flex flex-col items-end px-2 py-3 text-[12px] leading-tight tabular-nums"><s class="text-[11px] text-muted-foreground">$0.12</s><span class="font-semibold text-foreground">$0.06</span></div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums"><span class="text-muted-foreground">—</span></div>
<div class="px-3 py-3"><button type="button"></button></div>
</div>
<div role="row">
<div class="flex min-w-0 items-center gap-2 px-4 py-3"><span class="truncate text-[13px] font-medium normal-case tracking-normal text-foreground underline"><a data-state="closed">Laguna S 2.1</a></span><a aria-label="View Laguna S 2.1 deal details" class="shrink-0 border border-border bg-foreground px-1.5 py-0.5 text-[10px] font-semibold">FREE</a></div>
<div class="px-2 py-3 text-right text-[11px] text-muted-foreground tabular-nums">256K</div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums"><span class="font-semibold uppercase tracking-wide text-foreground">Free</span></div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums"><span class="font-semibold uppercase tracking-wide text-foreground">Free</span></div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums"><span class="font-semibold uppercase tracking-wide text-foreground">Free</span></div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums"><span class="text-muted-foreground">—</span></div>
<div class="px-3 py-3"><button type="button"></button></div>
</div>
<div role="row">
<div class="flex min-w-0 items-center gap-2 px-4 py-3"><span class="truncate text-[13px] font-medium normal-case tracking-normal text-foreground">Model</span></div>
<div class="px-2 py-3 text-right text-[11px] text-muted-foreground tabular-nums">Context</div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums">Input/M</div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums">Output/M</div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums">Cache Read</div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums">Cache Write</div>
<div class="px-3 py-3"><button type="button"></button></div>
</div>
"#;

    #[test]
    fn parses_plain_deal_and_free_rows_skips_header() {
        let m = parse_pricing_html(SAMPLE);
        assert_eq!(m.len(), 3);
        let dsv = m.get("DeepSeek V4.1 Flash").unwrap();
        assert!((dsv.input - 0.15).abs() < 1e-9);
        assert!((dsv.output - 0.60).abs() < 1e-9);
        assert!((dsv.cache_read - 0.003).abs() < 1e-9);
        assert!(dsv.cache_write.is_none());
        let mm = m.get("MiniMax M3").unwrap();
        // DEAL: the discounted price wins over the struck one.
        assert!((mm.input - 0.30).abs() < 1e-9);
        assert!((mm.output - 1.20).abs() < 1e-9);
        assert!((mm.cache_read - 0.06).abs() < 1e-9);
        let lag = m.get("Laguna S 2.1").unwrap();
        assert_eq!(lag.input, 0.0);
        assert_eq!(lag.output, 0.0);
        assert_eq!(lag.cache_read, 0.0);
    }

    #[test]
    fn empty_page_parses_to_empty() {
        assert!(parse_pricing_html("<html>no table here</html>").is_empty());
        assert!(parse_pricing_html("").is_empty());
    }

    #[test]
    fn price_matches_provider_prefixed_and_normalized_names() {
        let pr = Pricing {
            models: BTreeMap::from([(
                "DeepSeek V4.1 Flash".to_string(),
                ModelPrice {
                    input: 0.28,
                    output: 0.42,
                    cache_read: 0.028,
                    cache_write: None,
                    id: None,
                    peak: None,
                    off_peak: None,
                },
            )]),
            fetched_at: Some(1),
            source: PRICING_SOURCE,
        };
        // exact page name
        assert_eq!(pr.price("DeepSeek V4.1 Flash").unwrap().input, 0.28);
        // provider-prefixed ledger id
        assert_eq!(
            pr.price("deepseek/deepseek-v4.1-flash").unwrap().input,
            0.28
        );
        // bare ledger id
        assert_eq!(pr.price("deepseek-v4.1-flash").unwrap().input, 0.28);
        // unrelated name must not match
        assert!(pr.price("gpt-5").is_none());
    }

    #[test]
    fn normalize_drops_editorial_suffixes() {
        assert_eq!(normalize("DeepSeek V4 Flash (latest)"), "deepseek-v4-flash");
        assert_eq!(
            normalize("DeepSeek V4 Flash Vision (exp)"),
            "deepseek-v4-flash-vision"
        );
        assert_eq!(
            normalize("deepseek/deepseek-v4.1-flash"),
            "deepseek-deepseek-v4-1-flash"
        );
        assert_eq!(normalize("MiniMax M3"), "minimax-m3");
    }

    #[test]
    fn normalize_folds_dots_so_page_and_client_names_agree() {
        // '.' is a separator like any other once normalized.
        assert_eq!(normalize("Qwen 3.8 Flash"), "qwen-3-8-flash");
        assert_eq!(normalize("qwen3.8-flash"), "qwen3-8-flash");
        assert_eq!(
            normalize("DeepSeek V4.1 Flash"),
            normalize("deepseek-v4.1-flash")
        );
    }

    #[test]
    fn a_vendor_space_does_not_hide_the_price() {
        // The page writes "Qwen 3.8 Flash" (space after the vendor); clients
        // ask for "qwen3.8-flash". normalize() cannot reconcile those, so the
        // separator-free match key must — otherwise the row is silently
        // unpriced, which is what happened in production for 4.35M tokens.
        assert_eq!(match_key("Qwen 3.8 Flash"), match_key("qwen3.8-flash"));
        assert_eq!(match_key("Qwen 3.8 Flash"), "qwen38flash");
        let mut models = parse_pricing_html(SAMPLE);
        for m in models.values_mut() {
            m.id = None;
        }
        let p = Pricing {
            models,
            fetched_at: None,
            source: PRICING_SOURCE,
        };
        let found = p
            .price("deepseek-v4.1-flash")
            .expect("dotted name resolves");
        assert_eq!(found.input, 0.15);
    }

    /// A page row spelled with a vendor space, as the real table has it.
    const VENDOR_SPACE_SAMPLE: &str = r#"
<div role="row">
<div class="flex min-w-0 items-center gap-2 px-4 py-3"><span class="truncate text-[13px] font-medium normal-case tracking-normal text-foreground">Qwen 3.8 Flash</span></div>
<div class="px-2 py-3 text-right text-[11px] text-muted-foreground tabular-nums">256K</div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums"><span><span>$0.10</span></span></div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums"><span><span>$0.40</span></span></div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums"><span><span>$0.002</span></span></div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums"><span class="text-muted-foreground">—</span></div>
<div class="px-3 py-3"><button type="button"></button></div>
</div>"#;

    #[test]
    fn an_unknown_model_still_does_not_match_a_prefix() {
        // The looser key must stay exact on content: a name must never resolve
        // to a different model that merely shares a prefix.
        let mut models = parse_pricing_html(VENDOR_SPACE_SAMPLE);
        for m in models.values_mut() {
            m.id = None;
        }
        let p = Pricing {
            models,
            fetched_at: None,
            source: PRICING_SOURCE,
        };
        assert!(
            p.price("qwen3.8-flash").is_some(),
            "the exact name resolves"
        );
        assert!(
            p.price("qwen3.8-flash-thinking").is_none(),
            "a longer name must not"
        );
        assert!(p.price("gpt-5").is_none());
    }

    #[test]
    fn minute_of_day_is_read_from_the_rows_own_timestamp() {
        assert_eq!(
            minute_of_day_from_iso8601("2026-09-20T17:52:17.917Z"),
            Some(1072)
        );
        assert_eq!(
            minute_of_day_from_iso8601("2026-09-20T01:00:00.000Z"),
            Some(60)
        );
        assert_eq!(
            minute_of_day_from_iso8601("2026-09-20T00:00:00.000Z"),
            Some(0)
        );
        // Unrecognised shapes yield None so a caller can fall back rather than
        // silently pricing at minute 0.
        assert_eq!(minute_of_day_from_iso8601(""), None);
        assert_eq!(minute_of_day_from_iso8601("not-a-timestamp"), None);
        assert_eq!(minute_of_day_from_iso8601("2026-09-20 17:52:17"), None);
        assert_eq!(minute_of_day_from_iso8601("2026-09-20T25:00:00Z"), None);
        assert_eq!(minute_of_day_from_iso8601("2026-09-20T12:99:00Z"), None);
    }

    #[test]
    fn a_peak_row_is_priced_at_the_peak_rate_when_read_later() {
        // The point of pricing by the row's own minute: a request that ran in
        // the peak window keeps the peak price on a page opened during
        // off-peak hours. Before this, the page used "now", so the same row
        // changed price depending on when it was viewed.
        let body = r#"x\"models\":[{"id":"deepseek-v4.1-flash","name":"DeepSeek V4.1 Flash","category":"opensource","provider":"DeepSeek","inputCost":0.15,"outputCost":0.6,"cacheReadCost":0.003,"timeOfDay":{"effective":"2026-08-16T16:00:00Z","peak":{"inputCost":0.3,"outputCost":1.2,"cacheReadCost":0.006},"offPeak":{"inputCost":0.15,"outputCost":0.6,"cacheReadCost":0.003}}}]"#;
        let p = Pricing {
            models: parse_models_json(body),
            fetched_at: None,
            source: PRICING_SOURCE,
        };
        // The row ran at 02:00 UTC — inside the peak window (01:00-04:00).
        let minute = minute_of_day_from_iso8601("2026-09-20T02:00:00.000Z").expect("parseable");
        assert_eq!(minute, 120);
        let peak_cost = p
            .cost("deepseek-v4.1-flash", 1_000_000, 0, 0, 0, minute)
            .unwrap();
        let off_cost = p
            .cost("deepseek-v4.1-flash", 1_000_000, 0, 0, 0, 0)
            .unwrap();
        assert!(
            (peak_cost - 0.30).abs() < 1e-9,
            "peak hour must bill the peak rate"
        );
        assert!(
            (off_cost - 0.15).abs() < 1e-9,
            "off-peak bills the lower rate"
        );
    }

    #[test]
    fn parses_embedded_json_with_ids_and_peak_off_peak() {
        // A faithful miniature of the Next.js RSC payload chunk: quotes are
        // JS-escaped (\\") in the HTML source.
        let body = r#"x\"models\":[{"id":"deepseek-v4.1-flash","name":"DeepSeek V4.1 Flash","category":"opensource","provider":"DeepSeek","inputCost":0.15,"outputCost":0.6,"cacheReadCost":0.003,"timeOfDay":{"effective":"2026-08-16T16:00:00Z","peak":{"inputCost":0.3,"outputCost":1.2,"cacheReadCost":0.006},"offPeak":{"inputCost":0.15,"outputCost":0.6,"cacheReadCost":0.003}}}]"#;
        let m = parse_models_json(body);
        let p = m.get("DeepSeek V4.1 Flash").expect("model parsed");
        assert_eq!(p.id.as_deref(), Some("deepseek-v4.1-flash"));
        assert_eq!(p.input, 0.15);
        let pk = p.peak.expect("peak present");
        assert_eq!(pk.input, 0.3);
        assert_eq!(pk.output, 1.2);
        assert_eq!(p.off_peak.expect("off-peak present").input, 0.15);
    }

    #[test]
    fn rate_at_picks_peak_and_off_peak_windows() {
        let p = ModelPrice {
            input: 0.15,
            output: 0.6,
            cache_read: 0.003,
            cache_write: None,
            id: Some("deepseek-v4.1-flash".to_string()),
            peak: Some(RateSet {
                input: 0.3,
                output: 1.2,
                cache_read: 0.006,
            }),
            off_peak: Some(RateSet {
                input: 0.15,
                output: 0.6,
                cache_read: 0.003,
            }),
        };
        // 02:00 UTC -> peak window 01:00-04:00
        assert_eq!(p.rate_at(120).input, 0.3);
        // 07:00 UTC -> peak window 06:00-10:00
        assert_eq!(p.rate_at(420).input, 0.3);
        // 00:00, 05:00, 10:30, 23:59 -> off-peak
        assert_eq!(p.rate_at(0).input, 0.15);
        assert_eq!(p.rate_at(300).input, 0.15);
        assert_eq!(p.rate_at(630).input, 0.15);
        assert_eq!(p.rate_at(1439).input, 0.15);
        // flat models ignore the clock
        let flat = ModelPrice {
            input: 1.0,
            output: 2.0,
            cache_read: 0.1,
            cache_write: None,
            id: None,
            peak: None,
            off_peak: None,
        };
        assert_eq!(flat.rate_at(120).input, 1.0);
        assert_eq!(flat.rate_at(0).input, 1.0);
    }

    /// Live check against the official page: the embedded payload should
    /// yield the full model set with ids and the four DeepSeek peak/off-peak
    /// entries, and the grid should have patched cache-write rates in.

    #[test]
    #[ignore = "network"]
    fn live_fetch_parses_peak_off_peak_and_cache_write() {
        let p = fetch_latest().expect("live fetch");
        assert!(
            p.models.len() >= 58,
            "expected >=58 models, got {}",
            p.models.len()
        );
        let flash = p
            .price("deepseek/deepseek-v4.1-flash")
            .expect("v4.1-flash resolves via provider prefix");
        assert!(flash.id.is_some(), "embedded payload carries api ids");
        let (Some(pk), Some(op)) = (flash.peak, flash.off_peak) else {
            panic!("v4.1-flash must carry peak/off-peak");
        };
        assert_eq!(pk.input, 0.3);
        assert_eq!(pk.output, 1.2);
        assert_eq!(op.input, 0.15);
        assert_eq!(op.output, 0.6);
        // cache-write patched from the HTML grid where the payload has none
        let with_cw = p
            .models
            .values()
            .filter(|m| m.cache_write.is_some())
            .count();
        assert!(with_cw >= 10, "expected cache-write patch, got {}", with_cw);
    }

    #[test]
    fn cost_respects_peak_hour() {
        let p = Pricing {
            models: BTreeMap::from([(
                "DeepSeek V4.1 Flash".to_string(),
                ModelPrice {
                    input: 0.15,
                    output: 0.6,
                    cache_read: 0.003,
                    cache_write: None,
                    id: None,
                    peak: Some(RateSet {
                        input: 0.3,
                        output: 1.2,
                        cache_read: 0.006,
                    }),
                    off_peak: Some(RateSet {
                        input: 0.15,
                        output: 0.6,
                        cache_read: 0.003,
                    }),
                },
            )]),
            fetched_at: Some(1),
            source: PRICING_SOURCE,
        };
        // 1M prompt tokens, 0 cached, 0 completion.
        let off = p
            .cost("DeepSeek V4.1 Flash", 1_000_000, 0, 0, 0, 0)
            .unwrap();
        let peak = p
            .cost("DeepSeek V4.1 Flash", 1_000_000, 0, 0, 0, 120)
            .unwrap();
        assert!((off - 0.15).abs() < 1e-9);
        assert!((peak - 0.30).abs() < 1e-9);
    }

    #[test]
    fn cost_uses_cache_read_for_cached_tokens() {
        let m = parse_pricing_html(SAMPLE);
        let p = Pricing {
            models: m,
            fetched_at: None,
            source: PRICING_SOURCE,
        };
        // 1000 prompt, 800 cached, 100 completion on DeepSeek V4.1 Flash:
        // 200*0.15 + 800*0.003 + 100*0.60 = 30 + 2.4 + 60 = 92.4e-6 USD.
        let c = p.cost("DeepSeek V4.1 Flash", 1000, 800, 0, 100, 0).unwrap();
        assert!((c - 92.4e-6).abs() < 1e-12);
        // Unknown model → no cost.
        assert!(p.cost("nope", 1, 0, 0, 1, 0).is_none());
        // Cached > prompt cannot go negative: clamp keeps it sane.
        let c2 = p.cost("DeepSeek V4.1 Flash", 100, 800, 0, 0, 0).unwrap();
        assert!(c2 >= 0.0);
        // Free model costs nothing.
        let c3 = p.cost("Laguna S 2.1", 1000, 0, 0, 100, 0).unwrap();
        assert_eq!(c3, 0.0);
    }

    #[test]
    fn cost_bills_cache_creation_at_the_write_rate() {
        // DeepSeek V4.1 Flash has no cache-write column, so creation is billed
        // at the input rate (never understated).
        let m = parse_pricing_html(SAMPLE);
        let p = Pricing {
            models: m,
            fetched_at: None,
            source: PRICING_SOURCE,
        };
        // 1000 prompt, 300 cached-read, 200 cache-creation, 100 completion:
        // uncached = 1000-300-200 = 500 x 0.15 + 300 x 0.003 + 200 x 0.15
        //            + 100 x 0.60 = 75 + 0.9 + 30 + 60 = 165.9e-6 USD.
        let c = p
            .cost("DeepSeek V4.1 Flash", 1000, 300, 200, 100, 0)
            .unwrap();
        assert!((c - 165.9e-6).abs() < 1e-12);
    }

    #[test]
    fn cache_round_trips() {
        let dir = std::env::temp_dir().join("ccproxy-pricing-test");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let m = parse_pricing_html(SAMPLE);
        let p = Pricing {
            models: m,
            fetched_at: Some(1234),
            source: PRICING_SOURCE,
        };
        write_cache(&dir, &p);
        let back = read_cache(&dir).unwrap();
        assert_eq!(back.fetched_at, Some(1234));
        assert_eq!(back.models.len(), 3);
        assert_eq!(
            back.models.get("MiniMax M3").unwrap().input,
            p.models.get("MiniMax M3").unwrap().input
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_depends_on_age() {
        // Give the sample ids so the schema-upgrade check does not fire.
        let m: BTreeMap<String, ModelPrice> = parse_pricing_html(SAMPLE)
            .into_iter()
            .map(|(k, mut v)| {
                v.id = Some(k.clone());
                (k, v)
            })
            .collect();
        let fresh = Pricing {
            models: m.clone(),
            fetched_at: Some(crate::now_epoch_secs()),
            source: PRICING_SOURCE,
        };
        assert!(!fresh.stale());
        let old = Pricing {
            models: m,
            fetched_at: Some(crate::now_epoch_secs().saturating_sub(REFRESH_AFTER_SECS + 10)),
            source: PRICING_SOURCE,
        };
        assert!(old.stale());
        let none = Pricing {
            models: BTreeMap::new(),
            fetched_at: None,
            source: PRICING_SOURCE,
        };
        assert!(none.stale());
    }
}
